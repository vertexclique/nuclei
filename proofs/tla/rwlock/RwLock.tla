------------------------------ MODULE RwLock ------------------------------
(***************************************************************************)
(* The reader-writer lock of nuclei::sync (src/sync/raw_rwlock.rs, the    *)
(* acquire of src/sync/contend.rs and the drain role of src/sync/queue.rs),*)
(* which RwLock and ReentrantRwLock build on, at the grain of its atomic   *)
(* steps.                                                                  *)
(*                                                                          *)
(* In plain words: the state word holds the readers, a writer bit, an      *)
(* upgradable bit and an upgrading bit. A read enters while no writer      *)
(* holds the lock and no upgrade is under way; a write while it is free;   *)
(* an upgradable read while no writer or other upgradable read holds it;   *)
(* each even past queued waiters unless a handoff is owed, with the same   *)
(* bounded bypass as the semaphore. A release nudges the front waiter (and *)
(* the readers behind a sharing front). The holder of the write can        *)
(* downgrade it to a read in one step. The holder of the upgradable read   *)
(* can upgrade: it raises the upgrading bit, which holds new readers back, *)
(* and becomes the write at once if no reader is inside, or waits in its   *)
(* own slot until a pass sees the last reader gone.                        *)
(*                                                                          *)
(* What is modelled step by step: the state word and the queue's bits, the *)
(* owed count, the arrival stack, the waiting list and the upgrade's slot, *)
(* every node's phase and marks, each compare-and-swap of an attempt, an   *)
(* enqueue, a release, a downgrade, an upgrade and its finish, the         *)
(* owe-or-take and its settling, a withdrawal, and the drain's passes with *)
(* each grant and nudge. What is abstract, as in the semaphore model: the  *)
(* spin before queueing, the node's waker handshake, and the list's sweep. *)
(*                                                                          *)
(* States that differ only in what nobody reads again are kept as one: a   *)
(* step's own working values go back to where they started once read for   *)
(* the last time, and a node is forgotten (its marks go back to where they *)
(* started) once its future and the queue are both done with it: as a      *)
(* pass lets a withdrawn node go, and as a future takes or gives back its  *)
(* grant.                                                                  *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    Prog,       \* per task, its operations in order (see MC_RwLock)
    MaxBypass,  \* MAX_BYPASS
    Mutation    \* "none", or the name of the one rule this run takes away

\* The tasks are the names Prog gives programs to: 1, 2, ... for a sequence, or
\* model values (MC_RwLockSym), which a symmetry set can swap.
Tasks == DOMAIN Prog
\* One request per operation, and one for each upgrade's own wait.
Reqs == UNION {{<<t, j>> : j \in 1..Len(Prog[t])} : t \in Tasks}
        \cup UNION {{<<t, j + 100>> : j \in {i \in 1..Len(Prog[t]) : Prog[t][i] = "upgrade"}} : t \in Tasks}
NoReq == <<0, 0>>
Range(s) == {s[k] : k \in DOMAIN s}
Without(s, S) == SelectSeq(s, LAMBDA x : x \notin S)
\* The kind an operation first asks for.
KindOf(op) == CASE op = "read" -> "read" [] op = "drop_read" -> "read"
                [] op = "write" -> "write" [] op = "drop_write" -> "write" [] op = "downgrade" -> "write"
                [] OTHER -> "upg"

(* --algorithm RwLock
variables
    readers = 0, writer = FALSE, upgradable = FALSE, upgrading = FALSE,
    queuedBit = FALSE, nudgeOut = FALSE, handoffBit = FALSE, wakeFront = FALSE,
    draining = FALSE, dirty = FALSE,
    owedCount = 0,
    arrivals = <<>>,
    list = <<>>,
    upgSlot = NoReq,                     \* the upgrade's node while it waits
    phase = [r \in Reqs |-> "none"],
    nudged = [r \in Reqs |-> FALSE],
    owed = [r \in Reqs |-> FALSE],
    kind = [r \in Reqs |-> "read"],      \* "read", "write", "upg" or "upgrade"
    woken = [r \in Reqs |-> FALSE],
    enqueued = [r \in Reqs |-> FALSE],
    held = [t \in Tasks |-> "none"],     \* what each task holds: "none", "read", "write", "upg"
    pending = [r \in Reqs |-> "none"],   \* what a handoff entered for a node, not yet taken by its future
    \* records
    counted = [r \in Reqs |-> 0],
    readerWhileUpgrading = FALSE,
    bargedPastOwed = FALSE,
    upgradedOverReaders = FALSE,
    upgradeWaited = FALSE,
    downgraded = FALSE;

define
    State == [readers |-> readers, writer |-> writer, upgradable |-> upgradable,
              upgrading |-> upgrading]
    \* The word of a free lock: where a pass's sharing walk starts from, before it walks.
    FreeWord == [readers |-> 0, writer |-> FALSE, upgradable |-> FALSE, upgrading |-> FALSE]
    Admits(k, s) ==
        CASE k = "read" -> ~s.writer /\ (~s.upgrading \/ Mutation = "read_ignores_upgrading")
          [] k = "write" -> ~s.writer /\ ~s.upgradable /\ s.readers = 0
          [] OTHER -> ~s.writer /\ ~s.upgradable
    Enter(k, s) ==
        CASE k = "read" -> [s EXCEPT !.readers = @ + 1]
          [] k = "write" -> [s EXCEPT !.writer = TRUE]
          [] OTHER -> [s EXCEPT !.upgradable = TRUE]
    Holding(k) == {t \in Tasks : held[t] = k}
    Pending(k) == {r \in Reqs : pending[r] = k}
    \* The state word says exactly who holds what (a handoff's entry counts from the grant).
    StateMatches ==
        /\ readers = Cardinality(Holding("read")) + Cardinality(Pending("read"))
        /\ writer = (Holding("write") \cup Pending("write") # {})
        /\ upgradable = (Holding("upg") \cup Pending("upg") # {})
    \* A writer holds the lock alone; one upgradable read at most.
    Exclusion ==
        /\ Cardinality(Holding("write")) <= 1
        /\ Holding("write") # {} => Holding("read") = {} /\ Holding("upg") = {}
        /\ Cardinality(Holding("upg")) <= 1
    NoReaderWhileUpgrading == ~readerWhileUpgrading
    NoUpgradeOverReaders == ~upgradedOverReaders
    BypassBounded == \A r \in Reqs : counted[r] <= MaxBypass
    NoBargePastOwed == ~bargedPastOwed
    NoStrandedArrival == \A r \in Range(arrivals) : enqueued[r] => draining
    AllDone == \A t \in Tasks : pc[t] = "Done"
    FreeAtEnd == AllDone => ~writer /\ ~upgradable /\ ~upgrading /\ readers = 0
    \* Once every task ended, no waiter is counted owed a handoff any more: each
    \* owed mark was settled as its node left the queue, so nothing holds a
    \* later reader or writer back.
    OwedSettled == AllDone => owedCount = 0
    \* Every task ends. Once every task is at its end it stays there (an ended
    \* task takes no step, under every mutation too), so "in the end, all are
    \* ended" is the same as "all are ended again and again", which TLC checks
    \* directly, without the extra machinery an "in the end" needs.
    AllTasksEnd == []<>AllDone
    \* Witnesses.
    NoUpgradeWait == ~upgradeWaited
    NoDowngrade == ~downgraded
end define;

\* `forget`: the nodes in S are done with. Their futures will not look at them
\* again (a future looks at its node only while it asks), and the queue let
\* them go (it looks only at the nodes on its stack, its list and the upgrade's
\* slot), so their marks go back to where they started: two states that differ
\* only in what a finished node carried are one state.
macro forget(S) begin
    phase := [x \in Reqs |-> IF x \in S THEN "none" ELSE phase[x]];   \* as if never queued
    nudged := [x \in Reqs |-> nudged[x] /\ x \notin S];              \* no nudge left on it
    owed := [x \in Reqs |-> owed[x] /\ x \notin S];                  \* no owed mark left on it
    kind := [x \in Reqs |-> IF x \in S THEN "read" ELSE kind[x]];     \* its first kind again
    woken := [x \in Reqs |-> woken[x] /\ x \notin S];                \* no wake on its way
    enqueued := [x \in Reqs |-> enqueued[x] /\ x \notin S];          \* no enqueue recorded
end macro;

macro claimOrDirty(claimed) begin
    if draining then
        dirty := TRUE;
        claimed := FALSE;
    else
        draining := TRUE;
        claimed := TRUE;
    end if;
end macro;

\* (The withdrawn nodes a pass lets go, and the front a handoff looks at, are
\* named within their one step, so they are not kept between steps.)
procedure Drain()
variables wfBefore = FALSE, wl = {}, at = 1, sharing = FreeWord,
          raised = FALSE, sent = FALSE, noneLeft = FALSE;
begin
D_clear:
    wfBefore := wakeFront;
    dirty := FALSE;
    wakeFront := FALSE;
D_absorb:
    \* A dropped upgrade's node goes first; an arriving upgrade takes the slot.
    with gone = {r \in Range(arrivals) : phase[r] = "cancelled"},    \* the withdrawn arrivals
         left = {r \in Range(list) \cup Range(arrivals) : phase[r] = "cancelled"} do  \* all of them
        upgSlot := IF \E r \in Range(arrivals) \ gone : kind[r] = "upgrade"
                   THEN CHOOSE r \in Range(arrivals) \ gone : kind[r] = "upgrade"
                   ELSE IF upgSlot # NoReq /\ phase[upgSlot] = "cancelled" THEN NoReq ELSE upgSlot;
        list := SelectSeq(list \o arrivals, LAMBDA r : phase[r] # "cancelled" /\ kind[r] # "upgrade");
        arrivals := <<>>;
        \* Every withdrawn node on the stack and the list leaves both here (an
        \* upgrade is never withdrawn, so none of them is the upgrade's). The
        \* queue letting a node go settles its owed mark (`release_queued`, in
        \* src/sync/waiter/node.rs, forgives it), so each owed one is counted out.
        \* MUTATION absorb_after_sweep: the count looks at the list after it was
        \* swept, where no withdrawn node is left, so an owed node withdrawn on
        \* the list stays counted for good.
        if Mutation = "absorb_after_sweep" then
            owedCount := owedCount - Cardinality({r \in gone \cup {x \in Range(list) : phase[x] = "cancelled"}
                                                  : owed[r]});
        else
            owedCount := owedCount - Cardinality({r \in left : owed[r]});
        end if;
        forget(left);
    end with;
D_upgrade:
    \* The upgrade is granted once the last reader left.
    if upgSlot # NoReq /\ upgrading /\ (readers = 0 \/ Mutation = "upgrade_over_readers") then
        if phase[upgSlot] = "waiting" then
            phase[upgSlot] := "granted";
            wl := wl \cup {upgSlot};
        end if;
        upgSlot := NoReq;
    end if;
D_handoff:
    if handoffBit /\ list # <<>> then
      with front = Head(list) do    \* the front waiter, named for this step only
        if Admits(kind[front], State) then
            list := Tail(list);
            if owed[front] then
                owedCount := owedCount - 1;
            end if;
            if phase[front] = "waiting" then
                with s = Enter(kind[front], State) do
                    readers := s.readers;
                    writer := s.writer;
                    upgradable := s.upgradable;
                end with;
                phase[front] := "granted";
                pending[front] := kind[front];
                wl := wl \cup {front};
                if kind[front] = "write" then
                    goto D_nudgeStart;
                else
                    goto D_handoff;
                end if;
            else
                forget({front});    \* withdrawn: its future left it, the queue lets it go
                goto D_handoff;
            end if;
        end if;
      end with;
    end if;
D_nudgeStart:
    if ~(handoffBit /\ owedCount > 0) /\ list # <<>> then
        sharing := State;
        at := 1;
        raised := FALSE;
        sent := FALSE;
    else
        goto D_end;
    end if;
D_nudge:
    if at <= Len(list) then
        with node = list[at], fits = Admits(kind[list[at]], sharing) do
            if (at = 1 /\ (fits \/ wfBefore)) \/ (at > 1 /\ fits /\ kind[node] # "write"
                                                         /\ kind[Head(list)] # "write") then
                if ~raised then
                    nudgeOut := TRUE;
                    raised := TRUE;
                end if;
                if phase[node] = "waiting" then
                    if ~nudged[node] then
                        nudged[node] := TRUE;
                        wl := wl \cup {node};
                    end if;
                    sent := TRUE;
                end if;
            end if;
            if ~fits \/ kind[node] = "write" \/ (at > 1 /\ kind[Head(list)] = "write") then
                goto D_nudgeEnd;
            else
                sharing := Enter(kind[node], sharing);
                at := at + 1;
                goto D_nudge;
            end if;
        end with;
    end if;
D_nudgeEnd:
    if raised /\ ~sent then
        nudgeOut := FALSE;
    end if;
D_end:
    noneLeft := (\A k \in 1..Len(list) : phase[list[k]] = "cancelled") /\ upgSlot = NoReq;
    \* The nudge walk's values are read for the last time: back to their start.
    wfBefore := FALSE; at := 1; sharing := FreeWord; raised := FALSE; sent := FALSE;
D_exit:
    if dirty /\ Mutation # "exit_ignores_dirty" then
        noneLeft := FALSE;    \* read: the next pass computes it again
        goto D_clear;
    elsif ~noneLeft /\ wakeFront then
        goto D_clear;
    else
        draining := FALSE;
        if owedCount = 0 then
            handoffBit := FALSE;
        end if;
        if noneLeft then
            queuedBit := FALSE;
            wakeFront := FALSE;
        end if;
        noneLeft := FALSE;    \* read for the last time
    end if;
D_wake:
    \* A wake reaches the futures still asking; a forgotten node's wakes nobody.
    woken := [r \in Reqs |-> woken[r] \/ (r \in wl /\ phase[r] # "none")];
    return;
end procedure;

\* `released`: `Step::Wake(state | WAKE_FRONT)`.
procedure Released()
begin
Rd_ask:
    wakeFront := TRUE;
    if ~draining then
        draining := TRUE;
        call Drain();
        return;
    else
        return;
    end if;
end procedure;

\* The unlock of kind `uk` (`unlock_kind`); `own`: it is what task `self` holds.
procedure Unlock(uk = "read", own = TRUE)
variables wants = FALSE;
begin
U_unlock:
    \* `wants_drain` of the word the release saw; the last reader out also tells a waiting
    \* upgrade, and so does the upgradable read's release.
    wants := (queuedBit /\ (~nudgeOut \/ handoffBit))
             \/ (uk = "read" /\ readers = 1 /\ upgrading /\ Mutation # "no_upgrade_ready")
             \/ (uk = "upg" /\ upgrading);
    if uk = "read" then
        readers := readers - 1;
    elsif uk = "write" then
        writer := FALSE;
    else
        upgradable := FALSE;
        upgrading := FALSE;
    end if;
    if own then
        held[self] := "none";
    end if;
U_ask:
    if wants /\ Mutation # "release_no_pass" then
        call Released();
        return;
    else
        return;
    end if;
end procedure;

\* The contended acquire of kind `ak` for request `ar` (`Contend::poll`); `drop`:
\* the future is dropped at its first wait.
procedure Acquire(ak = "read", ar = NoReq, drop = FALSE)
variables bypass = 0, marked = FALSE, took = FALSE, seenHandoff = FALSE, claimed = FALSE,
          wasNudged = FALSE;
begin
Q_try:
    if Admits(ak, State) /\ ~handoffBit then
        readerWhileUpgrading := readerWhileUpgrading \/ (ak = "read" /\ upgrading);
        with s = Enter(ak, State) do
            readers := s.readers; writer := s.writer; upgradable := s.upgradable;
        end with;
        held[self] := ak;
        return;
    end if;
Q_enqueue:
    phase[ar] := "waiting";
    kind[ar] := ak;
    arrivals := Append(arrivals, ar);
Q_enqTr:
    queuedBit := TRUE;
    enqueued[ar] := TRUE;
    claimOrDirty(claimed);
Q_enqDrain:
    if claimed then
        claimed := FALSE;    \* read: back to its start
        call Drain();
    end if;
Q_park:
    if phase[ar] = "granted" then
        nudgeOut := FALSE;
        held[self] := ak;
        pending[ar] := "none";
        forget({ar});    \* granted and taken: the queue let it go at the grant
        return;
    elsif nudged[ar] then
        nudged[ar] := FALSE;
        nudgeOut := FALSE;
        goto Q_retry;
    elsif drop then
        goto Q_withdraw;
    end if;
Q_sleep:
    await woken[ar] \/ phase[ar] = "granted" \/ nudged[ar];
    woken[ar] := FALSE;
    goto Q_park;
Q_retry:
    if bypass + 1 = MaxBypass then
        goto Q_owe;
    end if;
Q_retryTake:
    if Admits(ak, State) /\ ~handoffBit then
        readerWhileUpgrading := readerWhileUpgrading \/ (ak = "read" /\ upgrading);
        with s = Enter(ak, State) do
            readers := s.readers; writer := s.writer; upgradable := s.upgradable;
        end with;
        held[self] := ak;
        goto Q_won;
    else
        seenHandoff := handoffBit;
        goto Q_lost;
    end if;
Q_owe:
    claimed := FALSE;
    took := FALSE;
    owedCount := owedCount + 1;
    marked := phase[ar] = "waiting";
    if marked then
        owed[ar] := TRUE;
    end if;
Q_oweTr:
    if ~marked then
        seenHandoff := handoffBit;
    elsif handoffBit then
        seenHandoff := TRUE;
    elsif Admits(ak, State) then
        with s = Enter(ak, State) do
            readers := s.readers; writer := s.writer; upgradable := s.upgradable;
        end with;
        held[self] := ak;
        took := TRUE;
        seenHandoff := FALSE;
    else
        seenHandoff := FALSE;
        handoffBit := TRUE;
        claimOrDirty(claimed);
    end if;
Q_oweDrain:
    if claimed then
        call Drain();
    end if;
Q_settle:
    claimed := FALSE;
    if ~marked \/ took \/ seenHandoff then
        if ~marked then
            owedCount := owedCount - 1;
        elsif phase[ar] = "waiting" /\ owed[ar] then
            owed[ar] := FALSE;
            owedCount := owedCount - 1;
        end if;
    end if;
Q_settleTr:
    if (~marked \/ took \/ seenHandoff) /\ handoffBit then
        claimOrDirty(claimed);
    end if;
Q_settleDrain:
    if claimed then
        claimed := FALSE;    \* read: back to its start
        call Drain();
    end if;
Q_after:
    if took then
        took := FALSE; marked := FALSE; seenHandoff := FALSE;    \* read for the last time
        goto Q_won;
    end if;
Q_lost:
    if ~seenHandoff then
        bypass := IF bypass + 1 > MaxBypass THEN MaxBypass ELSE bypass + 1;
        if phase[ar] = "waiting" then
            counted[ar] := counted[ar] + 1;
        end if;
    end if;
    marked := FALSE; took := FALSE; seenHandoff := FALSE;    \* read for the last time
    goto Q_park;
Q_won:
    if phase[ar] = "waiting" then
        wasNudged := nudged[ar];
        phase[ar] := "cancelled";
        goto Q_wdTr;
    elsif phase[ar] = "granted" then
        \* A grant came too: what it entered is given back (`give_back`).
        nudgeOut := FALSE;
        goto Q_undo;
    else
        return;
    end if;
Q_undo:
    pending[ar] := "none";
    forget({ar});    \* given back: its future is done with it, the queue let it go
    call Unlock(ak, FALSE);
    return;
Q_withdraw:
    if phase[ar] = "waiting" then
        wasNudged := nudged[ar];
        phase[ar] := "cancelled";
        goto Q_wdTr;
    elsif phase[ar] = "granted" then
        nudgeOut := FALSE;
        held[self] := ak;
        pending[ar] := "none";
        forget({ar});    \* granted as it is dropped: the queue let it go at the grant
        goto Q_dropGrant;
    else
        return;
    end if;
Q_dropGrant:
    call Unlock(ak, TRUE);
    return;
Q_wdTr:
    nudgeOut := FALSE;
    if wasNudged then
        wakeFront := TRUE;
    end if;
    wasNudged := FALSE;    \* read for the last time
    claimOrDirty(claimed);
Q_wdDrain:
    if claimed then
        call Drain();
        return;
    else
        return;
    end if;
end procedure;

\* The upgradable read's upgrade (`UpgradeWait::poll`), by task `self` holding it.
procedure Upgrade(ur = NoReq)
variables claimed = FALSE;
begin
G_raise:
    upgrading := TRUE;
G_try:
    \* `try_upgrade`: with no reader inside, the write at once.
    if readers = 0 then
        upgradable := FALSE;
        upgrading := FALSE;
        writer := TRUE;
        held[self] := "write";
        return;
    end if;
G_enqueue:
    phase[ur] := "waiting";
    kind[ur] := "upgrade";
    arrivals := Append(arrivals, ur);
    upgradeWaited := TRUE;
G_enqTr:
    queuedBit := TRUE;
    enqueued[ur] := TRUE;
    claimOrDirty(claimed);
G_enqDrain:
    if claimed then
        claimed := FALSE;    \* read: back to its start
        call Drain();
    end if;
G_park:
    await phase[ur] = "granted";
G_finish:
    \* `finish_upgrade`: one fetch_xor.
    upgradedOverReaders := upgradedOverReaders \/ readers > 0;
    upgradable := FALSE;
    upgrading := FALSE;
    writer := TRUE;
    held[self] := "write";
    forget({ur});    \* the upgrade's node: the slot let it go at the grant
    return;
end procedure;

\* `downgrade`: the write becomes one read in one step.
procedure Downgrade()
variables claimed = FALSE;
begin
W_down:
    writer := FALSE;
    readers := readers + 1;
    held[self] := "read";
    downgraded := TRUE;
    if queuedBit then
        wakeFront := TRUE;
        claimed := ~draining;
        draining := TRUE;
    end if;
W_downDrain:
    if claimed then
        call Drain();
        return;
    else
        return;
    end if;
end procedure;

fair process task \in Tasks
variables j = 1, op = "read";
begin
T_op:
    while j <= Len(Prog[self]) do
        op := Prog[self][j];
        call Acquire(KindOf(op), <<self, j>>, op \in {"drop_read", "drop_write"});
T_then:
        if held[self] = "none" then
            goto T_next;
        elsif op = "upgrade" then
            call Upgrade(<<self, j + 100>>);
        elsif op = "downgrade" then
            call Downgrade();
        end if;
T_release:
        if held[self] # "none" then
            call Unlock(held[self], TRUE);
        end if;
T_next:
        j := j + 1;
    end while;
end process;
end algorithm; *)
=============================================================================

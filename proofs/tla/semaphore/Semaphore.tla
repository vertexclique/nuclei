----------------------------- MODULE Semaphore -----------------------------
(***************************************************************************)
(* The barging semaphore of nuclei::sync (src/sync/raw_semaphore.rs, the  *)
(* acquire of src/sync/contend.rs and the drain role of src/sync/queue.rs),*)
(* which Semaphore, Mutex (one permit) and ReentrantMutex build on, at the  *)
(* grain of its atomic steps.                                              *)
(*                                                                          *)
(* In plain words: a caller takes the permits it asks for whenever they    *)
(* are free and no handoff is owed, even past waiters. When it cannot, it  *)
(* queues and waits. A release puts its permits back and, when somebody    *)
(* waits and no nudge is already on its way, asks for a pass. A pass, run  *)
(* by whoever holds the drain role, nudges the oldest waiters the free     *)
(* permits could serve (the front one whatever it asks for, after a        *)
(* release), and they try again. A nudged waiter that loses counts a       *)
(* bypass; the attempt that would be its last allowed loss is marked owed  *)
(* and, in the same compare-and-swap, either takes what it asks for or     *)
(* raises the handoff bit. While the bit is up nobody barges, and passes  *)
(* hand the free permits to the waiters in order. A waiter may give up    *)
(* (its future is dropped): what a grant gave it goes back. The semaphore  *)
(* can be closed: then nothing is admitted and every waiter is granted the *)
(* close.                                                                  *)
(*                                                                          *)
(* What is modelled step by step: the state word (free permits, the closed *)
(* bit, QUEUED, NUDGE_OUT, HANDOFF, WAKE_FRONT and the drain role's        *)
(* DRAINING and DIRTY), the owed count, the arrival stack and the waiting  *)
(* list, every waiter node (its phase, its nudge and owed marks, what a    *)
(* grant gave it), each compare-and-swap of an attempt, of an enqueue, of  *)
(* a release, of the owe-or-take and its settling, of a withdrawal and of  *)
(* the close, and the drain's passes with each grant and each nudge. What  *)
(* is abstract: the spin before queueing (more attempts of the same kind,  *)
(* which other callers' barging already covers); the node's waker          *)
(* handshake (a nudge or a grant reaches a waiting future: the waiter       *)
(* family's part); the list's sweep of withdrawn waiters, one step that    *)
(* takes them all (a waiter never comes back from withdrawn).              *)
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
    Permits,    \* the semaphore's permits (1: a Mutex)
    Prog,       \* per task, its operations in order: [op |-> "lock" or "cancel", n |-> permits]
    Closer,     \* a closer thread closes the semaphore at some point
    MaxBypass,  \* MAX_BYPASS (2 under loom; 16 in a build)
    Mutation    \* "none", or the name of the one rule this run takes away

\* The tasks are the names Prog gives programs to: 1, 2, ... for a sequence, or
\* model values (MC_SemaphoreSym), which a symmetry set can swap.
Tasks == DOMAIN Prog
CloserT == 100
Reqs == UNION {{<<t, j>> : j \in 1..Len(Prog[t])} : t \in Tasks}
NoReq == <<0, 0>>
RECURSIVE SumOver(_, _)
SumOver(f, D) == IF D = {} THEN 0
                 ELSE LET x == CHOOSE y \in D : TRUE IN f[x] + SumOver(f, D \ {x})
Range(s) == {s[j] : j \in DOMAIN s}
Without(s, S) == SelectSeq(s, LAMBDA x : x \notin S)

(* --algorithm Semaphore
variables
    \* ---- the state word ----
    free = Permits,
    closedBit = FALSE,
    queuedBit = FALSE,
    nudgeOut = FALSE,
    handoffBit = FALSE,
    wakeFront = FALSE,
    draining = FALSE,
    dirty = FALSE,
    \* ---- the queue ----
    owedCount = 0,
    arrivals = <<>>,
    list = <<>>,
    \* ---- one node per request ----
    phase = [r \in Reqs |-> "none"],   \* "none", "waiting", "granted" or "cancelled"
    nudged = [r \in Reqs |-> FALSE],
    owed = [r \in Reqs |-> FALSE],
    need = [r \in Reqs |-> 0],
    closedG = [r \in Reqs |-> FALSE],
    owned = [r \in Reqs |-> 0],        \* permits a grant gave the node, not yet taken by its future
    inPass = 0,                        \* permits a pass took for a node that withdrew at its grant
    woken = [r \in Reqs |-> FALSE],    \* a wake reached the node's future
    \* ---- the tasks ----
    heldBy = [t \in Tasks |-> 0],
    \* ---- records ----
    counted = [r \in Reqs |-> 0],      \* losses counted as bypasses, uncapped
    admittedAfterClose = FALSE,
    bargedPastOwed = FALSE,            \* a caller's own attempt took while the handoff bit was up
    raisedHandoff = FALSE,
    nudgePending = FALSE,              \* a nudge found its waiter still carrying the last one
    gaveBackGrant = FALSE,             \* a waiter that took on its own gave back a grant
    enqueued = [r \in Reqs |-> FALSE];  \* the waiter's enqueue transition happened

define
    Conservation ==
        free + SumOver(heldBy, Tasks) + SumOver(owned, Reqs) + inPass = Permits
    MutualExclusion == Permits = 1 => SumOver(heldBy, Tasks) <= 1
    NoAdmitAfterClose == ~admittedAfterClose
    BypassBounded == \A r \in Reqs : counted[r] <= MaxBypass
    NoBargePastOwed == ~bargedPastOwed
    \* With the drain role free, no waiter whose enqueue transition happened is still on the
    \* arrival stack: the pass that came after it, or the one its DIRTY forced, took it in.
    NoStrandedArrival == \A r \in Range(arrivals) : enqueued[r] => draining
    AllDone == \A t \in Tasks : pc[t] = "Done"
    PermitsBackAtEnd == AllDone => free = Permits
    \* Every task ends. Once every task is at its end it stays there (an ended
    \* task takes no step, under every mutation too), so "in the end, all are
    \* ended" is the same as "all are ended again and again", which TLC checks
    \* directly, without the extra machinery an "in the end" needs.
    AllTasksEnd == []<>AllDone
    \* Witnesses.
    NoHandoff == ~raisedHandoff
    NoNudgePending == ~nudgePending
    NoGrantGivenBack == ~gaveBackGrant
end define;

\* `forget`: the nodes in S are done with. Their futures will not look at them
\* again (a future looks at its node only while it asks), and the queue let
\* them go (it looks only at the nodes on its stack and list), so their marks go
\* back to where they started: two states that differ only in what a finished
\* node carried are one state.
macro forget(S) begin
    phase := [x \in Reqs |-> IF x \in S THEN "none" ELSE phase[x]];   \* as if never queued
    nudged := [x \in Reqs |-> nudged[x] /\ x \notin S];              \* no nudge left on it
    owed := [x \in Reqs |-> owed[x] /\ x \notin S];                  \* no owed mark left on it
    need := [x \in Reqs |-> IF x \in S THEN 0 ELSE need[x]];          \* asks for nothing
    closedG := [x \in Reqs |-> closedG[x] /\ x \notin S];            \* no close grant on it
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

(***************************************************************************)
(* The drain role's work (`WaitQueue::drain` and `RawSemaphore::pass`).   *)
(***************************************************************************)
procedure Drain()
\* (The withdrawn nodes a sweep takes, and the front a handoff looks at, are
\* named within their one step, so they are not kept between steps.)
variables wfBefore = FALSE, wl = {}, at = 1, budget = 0, raised = FALSE, sent = FALSE,
          noneLeft = FALSE;
begin
D_clear:
    wfBefore := wakeFront;
    dirty := FALSE;
    wakeFront := FALSE;
D_absorb:
    \* The arrival stack, oldest first; withdrawn nodes are let go (settling an owed one).
    with gone = {r \in Range(arrivals) : phase[r] = "cancelled"} do   \* the withdrawn ones
        list := list \o Without(arrivals, gone);
        arrivals := <<>>;
        owedCount := owedCount - Cardinality({r \in gone : owed[r]});
        forget(gone);    \* their futures left them, and now the queue lets them go
    end with;
D_tidy:
    \* The sweep: every withdrawn waiter on the list goes.
    with gone = {r \in Range(list) : phase[r] = "cancelled"} do   \* the withdrawn ones
        list := Without(list, gone);
        owedCount := owedCount - Cardinality({r \in gone : owed[r]});
        forget(gone);    \* their futures left them, and now the queue lets them go
    end with;
D_closed:
    if closedBit then
        \* Closed: every waiter is granted the close; the queue lets go of each.
        wl := wl \cup {r \in Range(list) : phase[r] = "waiting"};
        closedG := [r \in Reqs |-> closedG[r] \/ (r \in Range(list) /\ phase[r] = "waiting")];
        phase := [r \in Reqs |-> IF r \in Range(list) /\ phase[r] = "waiting" THEN "granted" ELSE phase[r]];
        owedCount := owedCount - Cardinality({r \in Range(list) : owed[r]});
        list := <<>>;
        goto D_end;
    end if;
D_handoff:
    \* While a handoff is owed, permits go to the waiters in order.
    if handoffBit /\ list # <<>> then
      with front = Head(list) do    \* the front waiter, named for this step only
        if free >= need[front] /\ ~closedBit then
            \* `take_for_waiter`, past the bit, then the grant.
            free := free - need[front];
            list := Tail(list);
            if owed[front] then
                owedCount := owedCount - 1;
            end if;
            if phase[front] = "waiting" then
                phase[front] := "granted";
                owned[front] := need[front];
                wl := wl \cup {front};
                goto D_handoff;
            else
                inPass := need[front];
                forget({front});    \* withdrawn: its future left it, the queue lets it go
            end if;
        else
            goto D_nudgeStart;
        end if;
      end with;
    else
        goto D_nudgeStart;
    end if;
D_handBack:
    \* Withdrawn at the grant: the permits go back, in a fetch_add of their own.
    free := free + inPass;
    inPass := 0;
    goto D_handoff;
D_nudgeStart:
    \* Nobody owed: nudge the oldest the free permits could serve, the front whatever it asks
    \* for after a release.
    if ~(handoffBit /\ owedCount > 0) /\ list # <<>> then
        budget := free;
        at := 1;
        raised := FALSE;
        sent := FALSE;
    else
        goto D_end;
    end if;
D_nudge:
    if at <= Len(list) /\ (need[list[at]] <= budget \/ (at = 1 /\ wfBefore)) then
        if ~raised then
            nudgeOut := TRUE;
            raised := TRUE;
        end if;
        if phase[list[at]] = "waiting" then
            if nudged[list[at]] then
                nudgePending := TRUE;
            else
                nudged[list[at]] := TRUE;
                wl := wl \cup {list[at]};
            end if;
            sent := TRUE;
        end if;
        budget := IF budget >= need[list[at]] THEN budget - need[list[at]] ELSE 0;
        at := at + 1;
        goto D_nudge;
    end if;
D_nudgeEnd:
    if raised /\ ~sent then
        nudgeOut := FALSE;
    end if;
D_end:
    noneLeft := \A k \in 1..Len(list) : phase[list[k]] = "cancelled";
    \* The nudge walk's values are read for the last time: back to their start.
    wfBefore := FALSE; at := 1; budget := 0; raised := FALSE; sent := FALSE;
D_exit:
    \* The compare-and-swap that gives the role up, or another pass.
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

(***************************************************************************)
(* `RawSemaphore::release(n)`.                                            *)
(***************************************************************************)
\* Gives back `back` permits: the caller's own (`fromReq` = NoReq), or what
\* a grant gave request `fromReq`.
procedure Release(back = 0, fromReq = NoReq)
variables wants = FALSE, claimed = FALSE;
begin
R_add:
    \* One fetch_add; `wants_drain` of the word it saw.
    wants := queuedBit /\ (~nudgeOut \/ handoffBit);
    free := free + back;
    if fromReq = NoReq then
        heldBy[self] := heldBy[self] - back;
    else
        owned[fromReq] := owned[fromReq] - back;
        forget({fromReq});    \* given back: its future is done with it, the queue let it go
    end if;
R_ask:
    if wants /\ Mutation # "release_no_pass" then
        \* `Step::Wake(state | WAKE_FRONT)`: the role, or only the request.
        wakeFront := TRUE;
        if ~draining then
            draining := TRUE;
            call Drain();
            return;
        else
            return;
        end if;
    else
        return;
    end if;
end procedure;

(***************************************************************************)
(* A future's acquire of `an` permits for request `ar` (`Contend::poll`); *)
(* `drop` says the future is dropped at its first wait.                    *)
(***************************************************************************)
procedure Acquire(an = 0, ar = NoReq, drop = FALSE)
variables bypass = 0, marked = FALSE, took = FALSE, seenHandoff = FALSE, claimed = FALSE,
          wasNudged = FALSE;
begin
Q_try:
    \* A fresh caller's attempt.
    if free >= an /\ (~closedBit \/ Mutation = "close_admits")
            /\ (~handoffBit \/ Mutation = "try_ignores_handoff") then
        bargedPastOwed := bargedPastOwed \/ handoffBit;
        admittedAfterClose := admittedAfterClose \/ closedBit;
        free := free - an;
        heldBy[self] := heldBy[self] + an;
        return;
    elsif closedBit then
        return;
    end if;
Q_enqueue:
    phase[ar] := "waiting";
    need[ar] := an;
    arrivals := Append(arrivals, ar);
Q_enqTr:
    \* `Step::Drain(state | QUEUED)`.
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
        if ~closedG[ar] then
            heldBy[self] := heldBy[self] + owned[ar];
            owned[ar] := 0;
        end if;
        forget({ar});    \* granted: the queue let it go at the grant, its future is done
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
    if free >= an /\ ~closedBit /\ ~handoffBit then
        free := free - an;
        heldBy[self] := heldBy[self] + an;
        goto Q_won;
    else
        seenHandoff := handoffBit;
        goto Q_lost;
    end if;
Q_owe:
    \* Counted, then marked (a granted node takes no mark).
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
    elsif free >= an /\ ~closedBit then
        free := free - an;
        heldBy[self] := heldBy[self] + an;
        took := TRUE;
        seenHandoff := FALSE;
    else
        \* The loss and the bit in one compare-and-swap.
        seenHandoff := FALSE;
        if Mutation # "owe_without_handoff" then
            handoffBit := TRUE;
            raisedHandoff := TRUE;
            claimOrDirty(claimed);
        end if;
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
        \* (record) A loss of a waiter still waiting; one granted meanwhile lost only to
        \* the permits taken for itself, which passes it over for nobody.
        if phase[ar] = "waiting" then
            counted[ar] := counted[ar] + 1;
        end if;
    end if;
    marked := FALSE; took := FALSE; seenHandoff := FALSE;    \* read for the last time
    goto Q_park;
Q_won:
    \* Took on its own: leave the queue (`Contend::cancel`).
    if phase[ar] = "waiting" then
        wasNudged := nudged[ar];
        phase[ar] := "cancelled";
        goto Q_wdTr;
    elsif phase[ar] = "granted" then
        \* A grant came too: what it gave goes back.
        nudgeOut := FALSE;
        gaveBackGrant := TRUE;
        goto Q_giveBack;
    else
        return;
    end if;
Q_withdraw:
    \* The future is dropped while it waits (`Contend::cancel`).
    if phase[ar] = "waiting" then
        wasNudged := nudged[ar];
        phase[ar] := "cancelled";
        goto Q_wdTr;
    elsif phase[ar] = "granted" /\ ~closedG[ar] then
        nudgeOut := FALSE;
        gaveBackGrant := TRUE;
        goto Q_giveBack;
    elsif phase[ar] = "granted" then
        nudgeOut := FALSE;
        forget({ar});    \* granted the close: the queue let it go at the grant
        return;
    else
        return;
    end if;
Q_giveBack:
    if Mutation = "keep_grant" then
        \* MUTATION keep_grant: the grant's permits stay with the node, never given back.
        return;
    else
        call Release(owned[ar], ar);
        return;
    end if;
Q_wdTr:
    \* `withdrawn`: `Step::Drain((state & !NUDGE_OUT) | (WAKE_FRONT if it was woken))`.
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

fair process task \in Tasks
variables j = 1, before = 0;
begin
T_op:
    while j <= Len(Prog[self]) do
        before := heldBy[self];
        call Acquire(Prog[self][j].n, <<self, j>>, Prog[self][j].op = "cancel");
T_release:
        if heldBy[self] > before then
            call Release(Prog[self][j].n, NoReq);
        end if;
T_next:
        j := j + 1;
    end while;
end process;

fair process closer \in (IF Closer THEN {CloserT} ELSE {})
variables claimed = FALSE;
begin
C_close:
    \* `close_step`: `Step::Drain(state | CLOSED)`, a draining transition.
    closedBit := TRUE;
    claimOrDirty(claimed);
C_drain:
    if claimed then
        call Drain();
    end if;
end process;
end algorithm; *)
=============================================================================

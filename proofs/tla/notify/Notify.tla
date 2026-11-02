------------------------------- MODULE Notify -------------------------------
(***************************************************************************)
(* Notify (src/sync/notify.rs: `Notify::notify_with`, `notify_waiters`,   *)
(* `take_permit`, `Policy::pass` for `Notify`, `Core::drive` and           *)
(* `Core::cancel`; on the drain role of src/sync/queue.rs), at the grain   *)
(* of its atomic steps.                                                    *)
(*                                                                          *)
(* In plain words: `notify_one` with nobody queued stores one permit (at   *)
(* most one); with somebody queued it counts one owed wake and asks for a  *)
(* pass, which grants the oldest waiter (`notify_last`: the newest).       *)
(* `notify_waiters` moves a generation counter on and asks for a pass,     *)
(* which grants every waiter of an older generation. A `Notified` takes    *)
(* the generation as it is made; polled, it is done if the generation      *)
(* moved since or it can take the permit with nobody queued ahead, and     *)
(* otherwise queues with its generation. The look at the generation and    *)
(* the take of the permit are two steps (`Core::drive`): a broadcast can   *)
(* land between them, and a permit taken after it is passed on with a      *)
(* `notify_one`, since the broadcast already owed this waiter its wake.    *)
(* A pass reads the generation once, too: a broadcast and then a           *)
(* `notify_one` that land while it runs are seen as an owed wake for a     *)
(* waiter the broadcast already woke, so the permit is lost (the witness  *)
(* `W_owed_to_woken`; the fix waits on a design choice).                   *)
(* A pass reads the generation after it took the arrivals, grants the      *)
(* queued waiters and the arrivals of an                                   *)
(* older generation, turns a stored permit into a wake for the front, and  *)
(* serves the owed wakes from the front (and the back); what it cannot     *)
(* serve becomes a permit again. A waiter dropped after a grant from       *)
(* `notify_one` (or `notify_last`) passes it on with a `notify_one` (or a *)
(* `notify_last`).                                                         *)
(*                                                                          *)
(* What is modelled step by step: the state word's PERMIT, QUEUED and the  *)
(* drain role's DRAINING and DIRTY, the generation, the owed counts, the   *)
(* arrival stack and the waiting list, each node's phase and payload, each *)
(* transition, each pass and each grant. What is abstract: the node's     *)
(* waker handshake (a grant reaches a waiting future: the waiter           *)
(* machinery's part) and the list's sweep of withdrawn nodes (one step).   *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    Waiters,   \* per waiter: "await" (polled until done), "drop" (polled once, then dropped),
               \* "early" (as "await", made before the notifier's first call) or "after" (as
               \* "await", made once the notifier's last call returned)
    Notifier,  \* the notifier's calls in order: "one", "last" or "waiters"
    NotifyAfterQueued, \* the notifier starts once every waiter has queued (or is done)
    Mutation   \* "none", or the name of the one rule this run takes away

W == 1..Len(Waiters)
NT == 100
NoNode == 0
BY_FIRST == -1
BY_LAST == -2
Range(s) == {s[i] : i \in DOMAIN s}
Without(s, S) == SelectSeq(s, LAMBDA x : x \notin S)

(* --algorithm Notify
variables
    permit = FALSE,
    queuedBit = FALSE,
    draining = FALSE,
    dirty = FALSE,
    generation = 0,
    pendingFirst = 0,
    pendingLast = 0,
    arrivals = <<>>,
    list = <<>>,
    phase = [w \in W |-> "none"],    \* "none", "waiting", "granted", "cancelled"
    payload = [w \in W |-> 0],       \* the node's generation, or BY_FIRST / BY_LAST once granted so
    wGen = [w \in W |-> 0],          \* the generation the Notified took as it was made
    made = [w \in W |-> FALSE],      \* the Notified exists
    done = [w \in W |-> FALSE],
    enqueued = [w \in W |-> FALSE],
    \* records
    ones = 0,          \* notify_one / notify_last calls made (passed-on ones included)
    servedByOne = 0,   \* waiters done through a notify_one / notify_last or a permit
    spurious = FALSE,  \* a waiter done with no notification since it was made
    passedOn = FALSE,  \* a dropped waiter passed its grant on
    eaten = FALSE;     \* a waiter the broadcast owed its wake kept a permit for itself

define
    \* A waiter is done only for a notification made since its Notified was: a generation
    \* that moved, a permit, or a grant.
    NoSpuriousWake == ~spurious
    \* No notify_one serves more than one waiter.
    NoOverServe == servedByOne <= ones
    \* With the drain role free, no waiter whose enqueue happened is left on the arrival stack.
    NoStrandedArrival == \A w \in Range(arrivals) : enqueued[w] => draining
    \* Every waiter made before the last notify_waiters, and still awaiting, ends done.
    AwaitersMadeBeforeAWaitersCallEnd ==
        <>[](\A w \in W : Waiters[w] = "await" /\ made[w] /\ wGen[w] # generation => done[w])
    \* With as many notify_one calls as awaiting waiters, made after them all, every one ends.
    EveryAwaiterEnds == <>[](\A w \in W : Waiters[w] = "await" => done[w])
    \* A permit goes to a waiter it can wake: one whose generation moved, so a broadcast
    \* already owes it its wake, never keeps a permit for itself.
    NoPermitEatenByOwed == ~eaten
    \* Every waiter made after the notifier's last call ends: the permit a notify_one left
    \* stored is still there for it. (Broken today by a pass that read the generation
    \* before a broadcast and serves a later notify_one to a waiter that broadcast woke.)
    LateAwaiterEnds == <>[](\A w \in W : Waiters[w] = "after" => done[w])
    \* Witnesses.
    NoPermitStored == ~permit
    NoPassOn == ~passedOn
end define;

macro claimOrDirty(claimed) begin
    if draining then
        dirty := TRUE;
        claimed := FALSE;
    else
        draining := TRUE;
        claimed := TRUE;
    end if;
end macro;

procedure Drain()
variables g = 0, first = 0, last = 0, gone = {}, wl = {}, n = NoNode;
begin
D_clear:
    dirty := FALSE;
D_take:
    \* The arrivals, then the generation: each arrival carries it or an older one.
    if Mutation = "generation_first" then
        g := generation;
    end if;
D_absorb:
    gone := {x \in Range(arrivals) : phase[x] = "cancelled"};
    list := list \o Without(arrivals, gone);
    arrivals := <<>>;
    if Mutation # "generation_first" then
        g := generation;
    end if;
D_expire:
    \* Every queued waiter of an older generation is granted; then the arrivals of one.
    gone := {x \in Range(list) : phase[x] = "cancelled"};
    wl := {x \in Range(list) : phase[x] = "waiting" /\ payload[x] # g};
    phase := [x \in W |-> IF x \in wl THEN "granted" ELSE phase[x]];
    list := Without(list, gone \cup wl);
D_owed:
    first := pendingFirst;
    pendingFirst := 0;
    last := pendingLast;
    pendingLast := 0;
D_permit:
    \* A stored permit is a wake for the front.
    if list # <<>> /\ permit then
        permit := FALSE;
        first := first + 1;
    end if;
D_first:
    if first > 0 /\ list # <<>> then
        n := Head(list);
        list := Tail(list);
        if phase[n] = "waiting" then
            phase[n] := "granted";
            payload[n] := BY_FIRST;
            wl := wl \cup {n};
            first := first - 1;
        end if;
        goto D_first;
    end if;
D_last:
    if last > 0 /\ list # <<>> then
        n := list[Len(list)];
        list := SubSeq(list, 1, Len(list) - 1);
        if phase[n] = "waiting" then
            phase[n] := "granted";
            payload[n] := BY_LAST;
            wl := wl \cup {n};
            last := last - 1;
        end if;
        goto D_last;
    end if;
D_leftover:
    \* What nobody could take is a permit again.
    if first + last > 0 then
        permit := TRUE;
    end if;
D_exit:
    if dirty /\ Mutation # "exit_ignores_dirty" then
        goto D_clear;
    else
        draining := FALSE;
        if \A k \in 1..Len(list) : phase[list[k]] = "cancelled" then
            queuedBit := FALSE;
        end if;
    end if;
D_wake:
    return;
end procedure;

\* `notify_with`: a permit with nobody queued, else an owed wake and a pass.
procedure NotifyWith(kind = "one")
variables claimed = FALSE;
begin
N_tr:
    ones := ones + 1;
    if ~queuedBit then
        permit := TRUE;
        return;
    end if;
N_owe:
    if kind = "one" then
        pendingFirst := pendingFirst + 1;
    else
        pendingLast := pendingLast + 1;
    end if;
N_drainTr:
    claimOrDirty(claimed);
N_drain:
    if claimed then
        call Drain();
        return;
    else
        return;
    end if;
end procedure;

procedure NotifyWaiters()
variables claimed = FALSE;
begin
NW_bump:
    generation := generation + 1;
NW_tr:
    \* A read-modify-write even with nobody queued; a pass when somebody is.
    if queuedBit /\ Mutation # "waiters_no_pass" then
        claimOrDirty(claimed);
    else
        claimed := FALSE;
    end if;
NW_drain:
    if claimed then
        call Drain();
        return;
    else
        return;
    end if;
end procedure;

fair process notifier = NT
variables i = 1;
begin
T_start:
    \* An "early" waiter is made before the first call.
    await (~NotifyAfterQueued \/ (\A w \in W : enqueued[w] \/ done[w] \/ Waiters[w] = "after"))
          /\ (\A w \in W : Waiters[w] = "early" => made[w]);
T_call:
    while i <= Len(Notifier) do
        if Notifier[i] = "waiters" then
            call NotifyWaiters();
        else
            call NotifyWith(Notifier[i]);
        end if;
T_next:
        i := i + 1;
    end while;
end process;

fair process waiter \in W
variables claimed = FALSE;
begin
W_make:
    \* An "after" waiter is made once the notifier's calls all returned.
    await Waiters[self] = "after" => pc[NT] = "Done";
    wGen[self] := generation;
    made[self] := TRUE;
W_gen:
    \* Done at once if the generation moved since it was made.
    if generation # wGen[self] then
        done[self] := TRUE;
        goto Done;
    end if;
W_take:
    \* Else the permit, if it is free with nobody queued (`take_permit`, one transition): a
    \* broadcast may have moved the generation since the look above.
    if permit /\ (~queuedBit \/ Mutation = "permit_past_queue") then
        permit := FALSE;                      \* the permit is this waiter's now
        done[self] := TRUE;                   \* and the waiter is done
    else
        goto W_enqueue;                       \* no permit: it queues
    end if;
W_recheck:
    \* After the take, the generation once more: moved, the broadcast owed this waiter its
    \* wake, so the permit goes on with a notify_one. MUTATION no_recheck: the waiter keeps it.
    if generation # wGen[self] then
        if Mutation = "no_recheck" then
            servedByOne := servedByOne + 1;   \* (record) the permit served this waiter ...
            eaten := TRUE;                    \* ... which the broadcast had served already
            goto Done;
        else
            call NotifyWith("one");           \* the permit goes to the next waiter, or back
            goto Done;
        end if;
    else
        servedByOne := servedByOne + 1;       \* (record) the permit served this waiter
        goto Done;
    end if;
W_enqueue:
    phase[self] := "waiting";
    payload[self] := wGen[self];
    arrivals := Append(arrivals, self);
W_enqTr:
    queuedBit := TRUE;
    enqueued[self] := TRUE;
    claimOrDirty(claimed);
W_enqDrain:
    if claimed then
        call Drain();
    end if;
W_park:
    \* "await", "early" and "after" wait; "drop" is dropped after its one poll.
    if Waiters[self] = "drop" then
        goto W_drop;
    end if;
W_wait:
    await phase[self] = "granted";
    if payload[self] \in {BY_FIRST, BY_LAST} then
        servedByOne := servedByOne + 1;
    elsif generation = wGen[self] then
        spurious := TRUE;
    end if;
    done[self] := TRUE;
    goto Done;
W_drop:
    \* `Core::cancel`: withdrawn, or granted; a grant of a notify_one is passed on.
    if phase[self] = "waiting" then
        phase[self] := "cancelled";
    elsif phase[self] = "granted" /\ payload[self] \in {BY_FIRST, BY_LAST}
            /\ Mutation # "drop_keeps_grant" then
        passedOn := TRUE;
        call NotifyWith(IF payload[self] = BY_FIRST THEN "one" ELSE "last");
        goto Done;
    else
        goto Done;
    end if;
W_wdTr:
    claimOrDirty(claimed);
W_wdDrain:
    if claimed then
        call Drain();
    end if;
end process;
end algorithm; *)
=============================================================================

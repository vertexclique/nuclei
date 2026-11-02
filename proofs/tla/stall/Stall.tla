-------------------------------- MODULE Stall --------------------------------
(***************************************************************************)
(* A waiter that is never polled again, at the front of a fair queue       *)
(* (src/sync/raw_semaphore.rs: `Policy::pass` for `RawSemaphore`, which   *)
(* wakes the front waiter after a release; src/sync/queue.rs:              *)
(* `wants_drain`, which skips the pass while that front's nudge is out;    *)
(* src/sync/internal.rs: `blocking`, which runs a nested                   *)
(* src/rt/shard/run.rs: `Shard::block_on` frame; src/rt/handoff/home.rs,   *)
(* which leaks a bound task woken after its home thread is gone).          *)
(*                                                                          *)
(* In plain words: one lock, held at the start by a holder that lets it    *)
(* go once. Waiters line up for it in arrival order. When the lock is let  *)
(* go, it is handed to the waiter at the front of the line (the code wakes *)
(* that front and serves nobody else until it acts), and that waiter must  *)
(* be polled to take it up, hold it and let it go to the next.             *)
(*                                                                          *)
(* Every other sync model makes each waiter a fair process: it is always   *)
(* polled again. The code does not promise that. A waiter in `Stalls` is   *)
(* never polled again once it queued: a `join!` branch while its task is   *)
(* blocked elsewhere, a future kept unpolled or forgotten, a bound task    *)
(* leaked because its home thread is gone. A pair <<o, n>> in `Nest` is a  *)
(* nested blocking wait on one thread: o's `blocking_*` frame runs the     *)
(* task of n, whose own `blocking_*` opens a frame on top of o's; a frame  *)
(* polls only its own future, so o is polled only once n has ended.        *)
(*                                                                          *)
(* What must never happen, for the queue to be live: a waiter that is      *)
(* polled again waiting for good. A stalled front, or a nested frame       *)
(* under the waiter behind it, makes that happen: the two witnesses show   *)
(* it, as the problem for the operator to decide.                          *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    Waiters,   \* how many waiters there are, numbered 1, 2, ...
    Stalls,    \* the waiters never polled again once they queued
    Nest,      \* pairs <<o, n>>: o's frame runs n, whose frame sits on top of o's
    Mutation   \* "none", or the name of the one rule this run takes away

\* Every waiter's number.
W == 1..Waiters
\* The holder that has the lock at the start; no waiter is numbered 0.
H == 0
\* Nobody holds the lock.
Nobody == -1
\* Waiter w may be polled: it is not stalled, and no frame of a nested waiter sits on its own.
CanRun(w, done) == w \notin Stalls /\ \A p \in Nest : p[1] = w => done[p[2]]
\* Waiter w may start: if a frame of another runs it, that other has queued already.
MayStart(w, queued) == \A p \in Nest : p[2] = w => queued[p[1]]

(* --algorithm Stall
variables
    holder = H,                         \* who holds the lock: the holder at the start
    line = <<>>,                        \* the waiters in line, oldest first
    handed = [w \in W |-> FALSE],       \* the lock was handed to w, not taken up yet
    queued = [w \in W |-> FALSE],       \* w has queued (or taken the lock at once)
    done = [w \in W |-> FALSE];         \* w held the lock and let it go

define
    \* At most one has the lock: its holder, or the one waiter it was handed to.
    Exclusion == Cardinality({w \in W : handed[w]}) + (IF holder = Nobody THEN 0 ELSE 1) <= 1
    \* Every waiter that is polled again ends, whatever the stalled ones do.
    OthersEnd == <>(\A w \in W \ Stalls : done[w])
end define;

\* The release (the guard's drop): the lock goes to the front of the line, or back to free.
\* MUTATION release_no_grant: the release frees the lock and hands it to nobody.
macro Release() begin
    if line # <<>> /\ Mutation # "release_no_grant" then
        handed[Head(line)] := TRUE;     \* the front waiter is handed the lock ...
        line := Tail(line);             \* ... and leaves the line
    end if;
    holder := Nobody;                   \* the one who let go holds nothing now
end macro;

\* The holder of the start: it lets the lock go once.
fair process first = H
begin
H_release:
    \* Its guard drops.
    Release();
end process;

\* A waiter: it queues for the lock, takes it up once handed over, holds it, and lets it go.
fair process waiter \in W
begin
W_arrive:
    \* It starts once whoever's frame runs it has queued; it takes a free lock with nobody in
    \* line at once, else it queues at the back.
    await MayStart(self, queued);
    queued[self] := TRUE;               \* (record) it has asked
    if holder = Nobody /\ line = <<>> /\ \A v \in W : ~handed[v] then
        holder := self;                 \* free, and nobody waits: taken at once
        goto W_release;
    else
        line := Append(line, self);     \* it waits in line
    end if;
W_take:
    \* It is polled again only if it can run; then it takes up the lock handed to it.
    await handed[self] /\ CanRun(self, done);
    handed[self] := FALSE;              \* taken up ...
    holder := self;                     \* ... it holds the lock
W_release:
    \* It lets the lock go, to the next in line.
    Release();
W_done:
    \* It ended.
    done[self] := TRUE;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

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
(* A frame that opens freezes the `blocking_*` waits of the frames under   *)
(* it on its thread (src/sync/internal.rs: `blocking` and its frames;      *)
(* src/rt/shard/run.rs: `Shard::block_on`, which freezes them as it        *)
(* opens): each frozen wait leaves the line as a dropped wait does,        *)
(* passing on the lock if it was handed it, so the lock goes to a waiter   *)
(* whose frame can run. When its frame is on top again it asks again, at   *)
(* the back of the line (the `no_freeze` mutation is the code before).     *)
(*                                                                          *)
(* What must never happen, for the queue to be live: a waiter that is      *)
(* polled again waiting for good. A stalled front makes that happen: the   *)
(* witness `W_never_polled` shows it, and it is the documented contract: a *)
(* future its user stops polling keeps its place in a fair queue until it  *)
(* is polled again or dropped (the sync primitives' docs,                  *)
(* `site/src/learn/sync.html`). A nested frame no longer does.             *)
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
    frozen = [w \in W |-> FALSE],       \* w's wait was withdrawn by a frame opened over it
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

\* A frame opening over o's freezes o's wait: o leaves the line, or, handed the lock already,
\* hands it on to the front of the line (or leaves it free), as a dropped wait does.
macro Freeze(o) begin
    if handed[o] then
        if line # <<>> then
            handed := [handed EXCEPT ![o] = FALSE, ![Head(line)] = TRUE];
            line := Tail(line);
        else
            handed[o] := FALSE;
        end if;
    else
        line := SelectSeq(line, LAMBDA x : x # o);
    end if;
    frozen[o] := TRUE;
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
W_open:
    \* It starts once whoever's frame runs it has queued; its own frame opens on top of that
    \* one and freezes the wait under it. MUTATION no_freeze: the wait under it stays queued.
    await MayStart(self, queued);
    if Mutation # "no_freeze" then
        with under = {p[1] : p \in {q \in Nest : q[2] = self}} do
            if under # {} then
                with o \in under do
                    if ~done[o] /\ ~frozen[o] then
                        Freeze(o);
                    end if;
                end with;
            end if;
        end with;
    end if;
W_arrive:
    \* It takes a free lock with nobody in line at once, else it queues at the back.
    queued[self] := TRUE;               \* (record) it has asked
    if holder = Nobody /\ line = <<>> /\ \A v \in W : ~handed[v] then
        holder := self;                 \* free, and nobody waits: taken at once
        goto W_release;
    else
        line := Append(line, self);     \* it waits in line
    end if;
W_take:
    \* It is polled again only if it can run; then it takes up the lock handed to it, or, its
    \* wait frozen while a frame sat over it, asks again.
    await (handed[self] \/ frozen[self]) /\ CanRun(self, done);
    if frozen[self] then
        frozen[self] := FALSE;          \* its frame is on top again ...
        goto W_arrive;                  \* ... and its wait asks again
    else
        handed[self] := FALSE;          \* taken up ...
        holder := self;                 \* ... it holds the lock
    end if;
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
VARIABLES holder, line, handed, queued, frozen, done, pc

(* define statement *)
Exclusion == Cardinality({w \in W : handed[w]}) + (IF holder = Nobody THEN 0 ELSE 1) <= 1

OthersEnd == <>(\A w \in W \ Stalls : done[w])


vars == << holder, line, handed, queued, frozen, done, pc >>

ProcSet == {H} \cup (W)

Init == (* Global variables *)
        /\ holder = H
        /\ line = <<>>
        /\ handed = [w \in W |-> FALSE]
        /\ queued = [w \in W |-> FALSE]
        /\ frozen = [w \in W |-> FALSE]
        /\ done = [w \in W |-> FALSE]
        /\ pc = [self \in ProcSet |-> CASE self = H -> "H_release"
                                        [] self \in W -> "W_open"]

H_release == /\ pc[H] = "H_release"
             /\ IF line # <<>> /\ Mutation # "release_no_grant"
                   THEN /\ handed' = [handed EXCEPT ![Head(line)] = TRUE]
                        /\ line' = Tail(line)
                   ELSE /\ TRUE
                        /\ UNCHANGED << line, handed >>
             /\ holder' = Nobody
             /\ pc' = [pc EXCEPT ![H] = "Done"]
             /\ UNCHANGED << queued, frozen, done >>

first == H_release

W_open(self) == /\ pc[self] = "W_open"
                /\ MayStart(self, queued)
                /\ IF Mutation # "no_freeze"
                      THEN /\ LET under == {p[1] : p \in {q \in Nest : q[2] = self}} IN
                                IF under # {}
                                   THEN /\ \E o \in under:
                                             IF ~done[o] /\ ~frozen[o]
                                                THEN /\ IF handed[o]
                                                           THEN /\ IF line # <<>>
                                                                      THEN /\ handed' = [handed EXCEPT ![o] = FALSE, ![Head(line)] = TRUE]
                                                                           /\ line' = Tail(line)
                                                                      ELSE /\ handed' = [handed EXCEPT ![o] = FALSE]
                                                                           /\ line' = line
                                                           ELSE /\ line' = SelectSeq(line, LAMBDA x : x # o)
                                                                /\ UNCHANGED handed
                                                     /\ frozen' = [frozen EXCEPT ![o] = TRUE]
                                                ELSE /\ TRUE
                                                     /\ UNCHANGED << line, 
                                                                     handed, 
                                                                     frozen >>
                                   ELSE /\ TRUE
                                        /\ UNCHANGED << line, handed, frozen >>
                      ELSE /\ TRUE
                           /\ UNCHANGED << line, handed, frozen >>
                /\ pc' = [pc EXCEPT ![self] = "W_arrive"]
                /\ UNCHANGED << holder, queued, done >>

W_arrive(self) == /\ pc[self] = "W_arrive"
                  /\ queued' = [queued EXCEPT ![self] = TRUE]
                  /\ IF holder = Nobody /\ line = <<>> /\ \A v \in W : ~handed[v]
                        THEN /\ holder' = self
                             /\ pc' = [pc EXCEPT ![self] = "W_release"]
                             /\ line' = line
                        ELSE /\ line' = Append(line, self)
                             /\ pc' = [pc EXCEPT ![self] = "W_take"]
                             /\ UNCHANGED holder
                  /\ UNCHANGED << handed, frozen, done >>

W_take(self) == /\ pc[self] = "W_take"
                /\ (handed[self] \/ frozen[self]) /\ CanRun(self, done)
                /\ IF frozen[self]
                      THEN /\ frozen' = [frozen EXCEPT ![self] = FALSE]
                           /\ pc' = [pc EXCEPT ![self] = "W_arrive"]
                           /\ UNCHANGED << holder, handed >>
                      ELSE /\ handed' = [handed EXCEPT ![self] = FALSE]
                           /\ holder' = self
                           /\ pc' = [pc EXCEPT ![self] = "W_release"]
                           /\ UNCHANGED frozen
                /\ UNCHANGED << line, queued, done >>

W_release(self) == /\ pc[self] = "W_release"
                   /\ IF line # <<>> /\ Mutation # "release_no_grant"
                         THEN /\ handed' = [handed EXCEPT ![Head(line)] = TRUE]
                              /\ line' = Tail(line)
                         ELSE /\ TRUE
                              /\ UNCHANGED << line, handed >>
                   /\ holder' = Nobody
                   /\ pc' = [pc EXCEPT ![self] = "W_done"]
                   /\ UNCHANGED << queued, frozen, done >>

W_done(self) == /\ pc[self] = "W_done"
                /\ done' = [done EXCEPT ![self] = TRUE]
                /\ pc' = [pc EXCEPT ![self] = "Done"]
                /\ UNCHANGED << holder, line, handed, queued, frozen >>

waiter(self) == W_open(self) \/ W_arrive(self) \/ W_take(self)
                   \/ W_release(self) \/ W_done(self)

(* Allow infinite stuttering to prevent deadlock on termination. *)
Terminating == /\ \A self \in ProcSet: pc[self] = "Done"
               /\ UNCHANGED vars

Next == first
           \/ (\E self \in W: waiter(self))
           \/ Terminating

Spec == /\ Init /\ [][Next]_vars
        /\ WF_vars(first)
        /\ \A self \in W : WF_vars(waiter(self))

Termination == <>(\A self \in ProcSet: pc[self] = "Done")

\* END TRANSLATION
=============================================================================

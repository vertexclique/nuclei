-------------------------------- MODULE Local --------------------------------
(***************************************************************************)
(* The single-shard semaphore (src/sync/local/semaphore.rs: `Acquire::poll`, *)
(* `Acquire::cancel`, `Semaphore::serve`, `try_acquire_many`; its line in  *)
(* src/sync/local/waiters.rs), which `sync::local::Mutex` and the bounded *)
(* `sync::local::mpsc` channel's slots are built on, at the grain of one   *)
(* poll or one drop of a task.                                             *)
(*                                                                         *)
(* In plain words: the tasks of one shard share a semaphore of two        *)
(* permits. A task's acquire takes its permits at once when nobody waits   *)
(* and they are free; otherwise it joins the back of the line and sleeps.  *)
(* A release gives permits back and serves the line: the front is granted  *)
(* when its whole need fits, then the next, stopping at the first that     *)
(* does not, and each one granted is woken. A woken task takes its grant   *)
(* when it is polled. A task's acquire may be dropped at any point: while  *)
(* it waits it leaves the line (and the line is served again when it was   *)
(* the front); after it was granted and before it took the grant, the     *)
(* permits pass on. An attempt that never waits takes nothing while       *)
(* anybody waits. The tasks run on one thread, so a poll or a drop is one  *)
(* step: the model interleaves whole steps in every order.                 *)
(*                                                                         *)
(* What must never happen: a permit lost or made up, a task served ahead   *)
(* of one that was already waiting when it asked, a waiter left asleep     *)
(* with its permits free at the front of the line, or a task never served. *)
(*                                                                         *)
(* What is abstract: wakers are the "granted" mark (single-threaded, a    *)
(* wake cannot race its registration); the close, which ends every waiter  *)
(* at once, is left out; each task acquires once and releases once.      *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    Needs,     \* each task's need: a function from {"a", "b", "c"}
    Cancels,   \* the tasks whose acquire may be dropped
    Tries,     \* the tasks that try once without waiting
    Mutation   \* "none", or the name of the one rule this run takes away

Tasks == {"a", "b", "c"}
\* The permits the semaphore holds in all.
Total == 2

\* How many waiters at the front of line `l` fit `p` free permits, one after another.
RECURSIVE Fit(_, _)
Fit(l, p) == IF l = <<>> \/ Needs[Head(l)] > p THEN 0 ELSE 1 + Fit(Tail(l), p - Needs[Head(l)])
\* The needs of a line, together.
RECURSIVE Sum(_)
Sum(l) == IF l = <<>> THEN 0 ELSE Needs[Head(l)] + Sum(Tail(l))
\* The pass (`serve`) with `p` free permits over line `l`: who is granted, what line is left,
\* and what stays free.
Granted(l, p) == {l[i] : i \in 1..Fit(l, p)}
LineAfter(l, p) == SubSeq(l, Fit(l, p) + 1, Len(l))
FreeAfter(l, p) == p - Sum(SubSeq(l, 1, Fit(l, p)))
\* The line without task `t`.
Without(l, t) == SelectSeq(l, LAMBDA x : x # t)

(* --algorithm Local
variables
    permits = Total,                       \* free permits
    line = <<>>,                           \* the waiting tasks, oldest first
    state = [t \in Tasks |-> "idle"],      \* "idle", "waiting", "granted", "holding" or "done"
    began = [t \in Tasks |-> 0],           \* when the task asked
    got = [t \in Tasks |-> 0],             \* when it got its permits (0: not yet)
    clock = 1,                             \* the order of events
    passedOn = FALSE,                      \* (record) a granted task was dropped and its permits passed on
    frontServed = FALSE,                   \* (record) the front waiter was dropped and the next was served
    setAside = FALSE;                      \* (record) a task waited with permits free (kept for the front)

define
    \* The permits a task holds, counting a grant it has not taken yet.
    Holds(t) == IF state[t] \in {"granted", "holding"} THEN Needs[t] ELSE 0
end define;

\* Serves line `l` with `p` free permits, `me` going `mine`: the front waiters that fit are granted
\* (and woken), the rest stay.
macro ServeLine(l, p, me, mine) begin
    with l0 = l, p0 = p do
        permits := FreeAfter(l0, p0);
        line := LineAfter(l0, p0);
        state := [t \in Tasks |-> IF t \in Granted(l0, p0) THEN "granted"
                                  ELSE IF t = me THEN mine ELSE state[t]];
        got := [t \in Tasks |-> IF t \in Granted(l0, p0) THEN clock ELSE got[t]];
    end with;
end macro;

fair process task \in Tasks
begin
T_ask:
    began[self] := clock;
    clock := clock + 1;
    if self \in Tries then
        \* An attempt that never waits takes the permits only when nobody waits and they are
        \* free. MUTATION try_barges: it takes free permits even while somebody waits.
        if (line = <<>> \/ Mutation = "try_barges") /\ permits >= Needs[self] then
            permits := permits - Needs[self];
            state[self] := "holding";
            got[self] := clock;
            goto T_release;
        else
            state[self] := "done";
            goto Done;
        end if;
    elsif line = <<>> /\ permits >= Needs[self] then
        \* Nobody waits and the permits are free: taken at once.
        permits := permits - Needs[self];
        state[self] := "holding";
        got[self] := clock;
        goto T_release;
    else
        \* Otherwise the task joins the back of the line and sleeps.
        setAside := setAside \/ permits > 0;
        line := Append(line, self);
        state[self] := "waiting";
    end if;
T_wait:
    either
        \* Woken by a grant, the task is polled and takes it.
        await state[self] = "granted";
        state[self] := "holding";
        goto T_release;
    or
        \* Its acquire is dropped while it waits, or after its grant and before it took it.
        await self \in Cancels /\ state[self] \in {"waiting", "granted"};
        if state[self] = "waiting" then
            \* It leaves the line; when it was the front the line is served again.
            \* MUTATION cancel_front_no_serve: the line is not served again.
            if Head(line) = self /\ Mutation # "cancel_front_no_serve" then
                frontServed := frontServed \/ Granted(Without(line, self), permits) # {};
                ServeLine(Without(line, self), permits, self, "done");
            else
                line := Without(line, self);
                state[self] := "done";
            end if;
        else
            \* Granted and dropped before taking it: its permits pass on, a release.
            \* MUTATION cancel_granted_keeps: the dropped grant keeps its permits.
            passedOn := TRUE;
            if Mutation = "cancel_granted_keeps" then
                state[self] := "done";
            else
                ServeLine(line, permits + Needs[self], self, "done");
            end if;
        end if;
        goto Done;
    end either;
T_release:
    \* The holder releases: its permits come back and the line is served.
    \* MUTATION release_no_serve: the permits come back and nobody is served.
    if Mutation = "release_no_serve" then
        permits := permits + Needs[self];
        state[self] := "done";
    else
        ServeLine(line, permits + Needs[self], self, "done");
    end if;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* Every permit is free or held (a grant not taken yet counts as held).
Conservation == permits + Holds("a") + Holds("b") + Holds("c") = Total

\* No task gets its permits while one that asked before it still waits.
FifoFair == \A x, y \in Tasks : ~(state[x] = "waiting" /\ began[x] < began[y] /\ got[y] > 0)

\* No waiter sleeps with its whole need free at the front of the line.
NoIdleFront == line = <<>> \/ Needs[Head(line)] > permits

Safe == Conservation /\ FifoFair /\ NoIdleFront

(***************************************************************************)
(* Witnesses.                                                              *)
(***************************************************************************)

\* A granted task is dropped before it takes its grant, and its permits pass on.
NoPassOn == ~passedOn
\* The front waiter is dropped and the next one is served.
NoFrontServed == ~frontServed
\* A task waits while permits are free, kept for the front of the line.
NoSetAside == ~setAside
=============================================================================

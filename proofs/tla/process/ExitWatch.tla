----------------------------- MODULE ExitWatch -----------------------------
(***************************************************************************)
(* A child's exit watched by one shard at a time, which hands it on as it *)
(* stops running (src/process/exit.rs: `Exit::poll_wait`, `try_wait`,     *)
(* `ensure_watched`, `went`, `unwatched`, `finish`; src/process/mod.rs:   *)
(* `Child::drop`; src/rt/shard/children.rs `let_go_of_children`;          *)
(* src/rt/shard/ring/exits.rs `end_exit`), at the grain of each atomic    *)
(* step.                                                                   *)
(*                                                                          *)
(* In plain words: the waiter looks for the outcome, registers its waker, *)
(* has the shard it runs on watch the child unless one does (a `try_wait` *)
(* before may have, with no waker), then looks once more before it sleeps. A watch on a ring is a `WAITID` the kernel *)
(* completes by reaping the child; a second watch whose child was reaped  *)
(* already would complete with ECHILD, an outcome that is wrong. Shard A  *)
(* may stop running: it cancels its watch and lets the child go only at   *)
(* that watch's completion (reaped, or cancelled), so the waiter, which   *)
(* then polls on shard B, never makes a second watch while the first is   *)
(* out. The waiter may instead drop its `Child`: a child no shard watches *)
(* is watched then, and one let go with its `Child` gone is watched on B. *)
(*                                                                          *)
(* A `try_wait` peeks: it stores the status of a child that exited without *)
(* reaping it, so a stored outcome does not mean the child was reaped. The *)
(* watcher's own outcome says so (`settled`), and only that ends the need  *)
(* for a watch. The `Child` goes in steps: it says it is gone, then claims *)
(* the watch if none is held (a swap of `watched`), then lets go of its    *)
(* reference. A shard that lets the child go clears `watched`, then looks  *)
(* whether the `Child` is gone, and if so claims the watch itself: each    *)
(* side publishes, then looks, so whichever looks second sees the other,   *)
(* and the swap leaves the watch to one of them.                           *)
(*                                                                          *)
(* What is abstract: a shard is its watch and its completion queue; the   *)
(* waker is a flag (`AtomicWaker`: a wake takes it).                       *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Mutation   \* "none", or the name of the one rule this run takes away

Shards == {"A", "B"}

(* --algorithm ExitWatch
variables
    exited = FALSE,       \* the child exited
    reaped = FALSE,       \* the kernel reaped it
    armed = [s \in Shards |-> FALSE],    \* a watch of shard s is out
    cancel = [s \in Shards |-> FALSE],   \* shard s asked its watch's cancel
    cq = [s \in Shards |-> "none"],      \* the completion shard s has to take
    aRuns = TRUE,         \* shard A runs; once it stops the waiter polls on B
    watched = FALSE,      \* a shard watches the child, or is about to
    outcome = "none",     \* "none", "status", or "echild" (wrong)
    waker = FALSE,
    woken = FALSE,
    gone = FALSE,         \* the `Child` said it is gone (`Exit::went`)
    holds = TRUE,         \* the `Child` still holds its reference to the exit
    settled = FALSE,      \* the watcher stored its outcome (`Exit::finish`)
    seen = FALSE,         \* the waiter took the outcome
    rewatched = FALSE;    \* (record) a watch let go was made again on B

define
    Here == IF aRuns THEN "A" ELSE "B"
    \* What tells that no watch is needed any more.
    \* MUTATION outcome_settles: any stored outcome does, a peeked one too (before the fix).
    Settled == IF Mutation = "outcome_settles" THEN outcome # "none" ELSE settled
end define;

\* The watcher's outcome is stored, the first one standing, the watcher done, and the waiter
\* woken.
macro Finish(o) begin
    if outcome = "none" then outcome := o; end if;
    settled := TRUE;
    if waker then waker := FALSE; woken := TRUE; end if;
end macro;

fair process child = "child"
begin
C_exit:
    exited := TRUE;
end process;

\* The kernel: a watch out completes once the child exited (reaping it, or
\* finding it reaped), or is cancelled.
fair process kernel = "kernel"
begin
K_loop:
    while TRUE do
        with s \in {x \in Shards : armed[x]} do
            either
                await exited;
                armed[s] := FALSE;
                cq[s] := IF reaped THEN "echild" ELSE "status";
                reaped := TRUE;
            or
                await cancel[s];
                armed[s] := FALSE;
                cancel[s] := FALSE;
                cq[s] := "cancelled";
            end either;
        end with;
    end while;
end process;

\* A shard takes its completions; A may stop running at any point.
fair process shard \in Shards
variables c = "none", counted = FALSE;
begin
S_loop:
    while TRUE do
        either
            await cq[self] # "none";
            c := cq[self];
            cq[self] := "none";
S_take:
            if c = "cancelled" then
                \* Its watch ended: the child is let go now.
                \* MUTATION count_orphan: whether the `Child` is gone is read off the count of
                \* the exit's holders, before `watched` is cleared (before the fix): the
                \* `Child` may still hold its reference, then claim nothing and let go.
                if Mutation = "count_orphan" then counted := ~holds; end if;
S_unwatch:
                watched := FALSE;
S_look:
                \* Then a look whether the `Child` went: if so the shard claims the watch,
                \* unless the `Child` did, and has B watch the child; else the waiter, if one
                \* waits, is woken to watch it where it polls next.
                if (Mutation = "count_orphan" /\ counted)
                    \/ (Mutation # "count_orphan" /\ gone) then
                    if ~Settled /\ ~watched then
                        watched := TRUE;
                        armed["B"] := TRUE;
                        rewatched := TRUE;
                    end if;
                elsif waker then
                    waker := FALSE;
                    woken := TRUE;
                end if;
            else
                Finish(c);
            end if;
        or
            await self = "A" /\ aRuns;
            aRuns := FALSE;
            if armed["A"] then
                cancel["A"] := TRUE;
                \* MUTATION early_let_go: the child is let go with the cancel
                \* asked, its watch still out.
                if Mutation = "early_let_go" then
                    watched := FALSE;
                    if waker then waker := FALSE; woken := TRUE; end if;
                end if;
            end if;
        end either;
    end while;
end process;

fair process waiter = "waiter"
begin
W_try:
    \* A `try_wait` first, or not: it has a shard watch the child unless one does, with no
    \* waker registered ...
    either
        goto W_check;
    or
        if ~Settled /\ ~watched then
            watched := TRUE;
            armed[Here] := TRUE;
        end if;
    end either;
W_peek:
    \* ... then peeks: the status of a child that exited is stored, the child unreaped.
    if exited /\ ~reaped /\ outcome = "none" then
        outcome := "status";
    end if;
W_check:
    while outcome = "none" do
        either
W_register:
            waker := TRUE;
W_watch:
            if ~Settled /\ ~watched then
                watched := TRUE;
                armed[Here] := TRUE;
            end if;
W_recheck:
            \* MUTATION no_recheck: the waiter sleeps without looking again.
            if outcome = "none" \/ Mutation = "no_recheck" then
W_sleep:
                \* A task sleeps until its waker is woken.
                await woken;
                woken := FALSE;
            end if;
        or
            \* The `Child` goes before the outcome came.
            goto W_gone;
        end either;
    end while;
W_seen:
    \* The waiter takes the outcome ...
    seen := TRUE;
W_ensure:
    \* ... and one a peek stored has the child watched, so it is reaped.
    if ~Settled /\ ~watched then
        watched := TRUE;
        armed[Here] := TRUE;
    end if;
W_gone:
    \* The `Child` goes: it says so (`Exit::went`) ...
    gone := TRUE;
W_claim:
    \* ... then claims the watch: a child no shard watches is watched now.
    \* MUTATION unwatched_orphan: a dropped `Child`'s child is left.
    if ~Settled /\ ~watched /\ Mutation # "unwatched_orphan" then
        watched := TRUE;
        armed[Here] := TRUE;
    end if;
W_release:
    \* ... then lets go of its reference.
    holds := FALSE;
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

\* One watch at most is ever out.
OneWatch == ~(armed["A"] /\ armed["B"])

\* The outcome is never the ECHILD a second reaper would find.
NoEchild == outcome # "echild"

\* The child is reaped once its `Child` is gone, whether the waiter took the outcome first or
\* not.
Settles == <>(reaped /\ gone)

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A watch cancelled as A stopped is made again on B for a child whose `Child` went.
NoRewatch == ~rewatched
\* The waiter sees the outcome after A stopped running.
NoSeenOnB == ~(seen /\ ~aRuns)
\* The waiter takes the status a `try_wait` peeked before the watcher stored its own.
NoPeekTaken == ~(seen /\ ~settled)
=============================================================================

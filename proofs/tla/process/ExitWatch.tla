----------------------------- MODULE ExitWatch -----------------------------
(***************************************************************************)
(* A child's exit watched by one shard at a time, which hands it on as it *)
(* stops running (src/process/exit.rs; src/rt/shard/children.rs;          *)
(* src/rt/shard/ring/exits.rs), at the grain of each atomic step.         *)
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
    orphan = FALSE,       \* the `Child` is gone
    seen = FALSE,         \* the waiter took the outcome
    rewatched = FALSE;    \* (record) a watch let go was made again on B

define
    Here == IF aRuns THEN "A" ELSE "B"
end define;

\* The outcome is stored, the first one standing, and the waiter woken.
macro Finish(o) begin
    if outcome = "none" then outcome := o; end if;
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
variables c = "none";
begin
S_loop:
    while TRUE do
        either
            await cq[self] # "none";
            c := cq[self];
            cq[self] := "none";
S_take:
            if c = "cancelled" then
                \* Its watch ended: the child is let go now (an orphan's to
                \* be watched on B).
                watched := orphan;
                if orphan then
                    armed["B"] := TRUE;
                    rewatched := TRUE;
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
    \* A `try_wait` first, or not: it has a shard watch the child, with no
    \* waker registered.
    either
        skip;
    or
        watched := TRUE;
        armed[Here] := TRUE;
    end either;
W_check:
    while outcome = "none" do
        either
W_register:
            waker := TRUE;
W_watch:
            if ~watched then
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
            \* The `Child` goes: a child no shard watches is watched now.
            orphan := TRUE;
            \* MUTATION unwatched_orphan: a dropped `Child`'s child is left.
            if ~watched /\ Mutation # "unwatched_orphan" then
                watched := TRUE;
                armed[Here] := TRUE;
            end if;
            goto Done;
        end either;
    end while;
    seen := TRUE;
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

\* The child is reaped, and a waiter that stays sees the outcome.
Settles == <>(reaped /\ (orphan \/ seen))

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A watch cancelled as A stopped is made again on B for an orphan.
NoRewatch == ~rewatched
\* The waiter sees the outcome after A stopped running.
NoSeenOnB == ~(seen /\ ~aRuns)
=============================================================================

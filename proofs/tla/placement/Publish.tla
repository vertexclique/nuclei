------------------------------- MODULE Publish -------------------------------
(***************************************************************************)
(* A worker's start that publishes its shard to placement, racing a spawn, *)
(* while the worker's thread may fail to start (src/rt/workers.rs:         *)
(* `Workers::start_one`, `spawn_thread`, `place`; src/rt/shard.rs:         *)
(* `Shared::stop`; src/rt/shard/going.rs: `Drop for Shard`; src/rt/       *)
(* runtime.rs: `spawn_with`, `spawn_here`).                                *)
(*                                                                          *)
(* Who acts. The starter, which makes the worker's shard, asks the system  *)
(* for its thread and publishes the shard in its slot, where placement     *)
(* finds it. A spawner on another thread, which places a task: on the      *)
(* shard if placement finds it published and taking tasks, else (with no   *)
(* other worker) on the spawning thread's own shard, which runs it. The    *)
(* worker's thread, once started, runs what its shard queues.             *)
(*                                                                          *)
(* What can happen. The thread start may fail: the starter stops the       *)
(* shard and drops it with its run, and the shard drops what it queues.   *)
(* The slot is published only once the thread took its run, so a shard    *)
(* placement finds always has a thread.                                    *)
(*                                                                          *)
(* What must never happen. A task whose spawn answered is dropped unrun   *)
(* (`NoSilentLoss`). And the task runs in the end.                         *)
(*                                                                          *)
(* What is abstract. One spawn, one worker, one start. The spawn's looks,  *)
(* count and push are one step (tla/placement/Placement.tla checks them    *)
(* against a drain and a handoff). The publish runs under the lock of the  *)
(* list a shutdown joins, in `spawn_thread`'s `started`, once the thread   *)
(* is on the list (tla/teardown/Grow.tla checks that against a drop).      *)
(***************************************************************************)
EXTENDS TLC                                                                  \* TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Mutation    \* "none", or the name of the one rule this run takes away

(* --algorithm Publish                                                       \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    published = FALSE,  \* the shard is in its slot, where placement finds it
    stopped = FALSE,    \* `Shared::stop`: it takes no task, and runs no more
    up = FALSE,         \* the worker's thread took its run
    failed = FALSE,     \* (record) the thread did not start
    task = "none";      \* the spawn's task: "none", "queued" (on the shard), "own" (on the spawner's), "ran", "dropped"

define                                                                       \* names the properties use:
    \* A task whose spawn answered is never dropped unrun.
    NoSilentLoss == task # "dropped"                                         \* never lost
    \* The task runs in the end.
    Runs == <>(task = "ran")                                                 \* it runs
    \* Witness: the thread does not start.
    NoStartFails == ~failed                                                  \* claims to: never
end define;                                                                  \* end of the names

\* The starter (`Workers::start_one`).
fair process starter = "P"                                                   \* weakly fair: it goes on
begin                                                                        \* its steps:
P_early:                                                                     \* the shard is made
    \* MUTATION publish_first: the slot is published before the thread is asked for, as before the fix.
    if Mutation = "publish_first" then published := TRUE; end if;           \* (mutated) placement finds it now
P_spawn:                                                                     \* `spawn_thread`
    either up := TRUE;                                                       \* the thread starts and takes its run
    or failed := TRUE;                                                       \* it does not start
    end either;                                                              \* one of the two
P_after:                                                                     \* what the start's outcome leads to
    if up then                                                               \* a thread runs the shard:
        published := TRUE;                                                   \* the slot is published (again, mutated)
    else                                                                     \* no thread: the shard stops and goes with its run
        stopped := TRUE;                                                     \* `Shared::stop`
        if task = "queued" then task := "dropped"; end if;                   \* `Drop for Shard`: what it queues is dropped
    end if;                                                                  \* done
end process;                                                                 \* end of the starter

\* The spawner (`spawn_with`: `Workers::place`, then the shard, or `spawn_here`).
fair process spawner = "S"                                                   \* weakly fair: it spawns
begin                                                                        \* its one step:
S_place:                                                                     \* placement, the push, the answer
    if published /\ ~stopped then task := "queued";                          \* on the worker's shard
    else task := "own";                                                      \* no worker takes it: the spawner's own shard
    end if;                                                                  \* the spawn answered
end process;                                                                 \* end of the spawner

\* The worker's thread, and the spawner's own shard: each runs what it holds.
fair process runs = "R"                                                      \* weakly fair: they run
begin                                                                        \* its steps:
R_run:                                                                       \* a run of the task where it is
    await (task = "queued" /\ up) \/ task = "own" \/ task = "dropped"        \* one to run, or lost
          \/ (pc["S"] = "Done" /\ task = "none");                            \* (never placed: nothing to run)
    if task \in {"queued", "own"} then task := "ran"; end if;                \* it runs
end process;                                                                 \* end of the runs
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

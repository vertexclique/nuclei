------------------------------ MODULE Placement ------------------------------
(***************************************************************************)
(* A task placed on a worker the caller names, while that worker is        *)
(* drained and while its shard is handed to another thread.               *)
(*                                                                          *)
(* The Rust it models: `spawn_on_worker` and `worker_shard`                *)
(* (src/rt/runtime/placement.rs); `Shared::drain`, `Shared::takes_tasks`,  *)
(* `Shared::may_exit`, `Shared::stopped`, `Shared::push_remote` and        *)
(* `Shared::last_task_done` (src/rt/shard.rs); `Drop for Shard`, which     *)
(* drops what the shard still queues (src/rt/shard/going.rs);             *)
(* `Counted::new`, `Drop for Counted` and `Sched::schedule`                *)
(* (src/rt/sched.rs); the monitor's `take` (src/rt/handoff/monitor.rs),    *)
(* which moves the shard to another thread; and `run_shard` with           *)
(* `finish_home` (src/rt/workers.rs), where a thread the shard was taken   *)
(* from runs the tasks bound to it.                                        *)
(*                                                                          *)
(* In plain words. Who acts: a spawner per task, which places its task on  *)
(* worker k; a stopper, which drains worker k; the shard of worker k with  *)
(* its loop, run by one thread at a time (its runner); the monitor, which  *)
(* may hand the shard to the spare thread while the runner is stuck in a  *)
(* task's poll; and the thread the shard was taken from, which finishes    *)
(* that one poll. A spawn looks at the shard's `draining` flag (refusing   *)
(* if it is up), makes the task, which counts it in the shard's `load`,   *)
(* runs a sequentially consistent fence and looks at `draining` again     *)
(* (dropping the task unrun and refusing if it is up now), then pushes it *)
(* on the shard and looks at `stopped` (a stopped shard's inbox is        *)
(* dropped), and answers Ok. The drain raises `draining`. The loop takes  *)
(* a task and polls it, or looks whether it may exit: `draining` first,   *)
(* then `load` in a second load; up and zero, it stops and drops whatever  *)
(* it still queues. A task's count goes when the task completes or is      *)
(* dropped. A poll may end Pending: the task is woken and pushed back.    *)
(*                                                                          *)
(* What must never happen: a task whose spawn answered Ok dropped unrun   *)
(* (`NoSilentLoss`); a refused task polled (`RefusedNeverRuns`); a placed *)
(* task polled anywhere but on shard k by the thread that runs it then    *)
(* (`RunsOnItsWorker`). Why it holds: the count comes before the second   *)
(* look and the exit reads `draining` before `load`, so a loop that exits *)
(* read `load` after every spawn that saw `draining` down had counted its *)
(* task; and the queue is the shard's, so it moves with the shard.        *)
(*                                                                          *)
(* What is abstract. Every step is one atomic load, store or read-modify- *)
(* write, sequentially consistent: the fences of `spawn_on_worker` and     *)
(* `drain` order nothing more here. The stop waker is not modelled: the   *)
(* loop may look whether it may exit at any moment it is not in a poll,    *)
(* which includes every look the code makes (the first poll of the        *)
(* `stopped` future, and each after a wake by `drain` or                   *)
(* `last_task_done`); so `drain` is its store, and its look at `load`,    *)
(* which only decides a wake, and `last_task_done`'s wake are left out.   *)
(* The shard's inbox and its own queues are one set of tasks. The shard's  *)
(* drop is one step. A handoff is one step (the monitor's claim, fence,   *)
(* look and take are `tla/handoff`'s). The spawners run on threads that    *)
(* run no shard, so a schedule is `push_remote`. `Shared::stop`, the       *)
(* runtime's shutdown, which drops every task by design, is not modelled. *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets  \* the standard modules used

CONSTANTS  \* what each configuration sets
    Tasks,        \* the tasks placed on worker k, each by a spawner of its own: a set of numbers
    MaxPolls,     \* the most polls one task takes: each one before its last may end Pending
    MaxHandoffs,  \* how many times the monitor may hand shard k to another thread
    Mode          \* "ok", or the one rule this run breaks (see the configurations)

Threads == {1, 2}       \* thread 1 runs shard k first; thread 2 waits as a spare
Other(th) == 3 - th     \* the thread that is not th

\* The steps of a spawn, in the order `spawn_on_worker` takes them: the first look at
\* `draining` (`worker_shard`), the count (`Counted::new`), the fence and the second look, the
\* push onto the shard's inbox and `push_remote`'s look at `stopped`. Each mutation reorders them.
\* MUTATION no_first_look: no first look (it is no guard: P_no_first_look must still pass).
\* MUTATION no_second_look: no second look after the count.
\* MUTATION look_before_count: the second look before the count.
\* MUTATION refuse_after_push: the second look after the task was scheduled.
Steps ==  \* the steps of a spawn, by mode
    CASE Mode = "no_first_look"     -> <<"count", "look2", "push", "check">>           \* no first look
      [] Mode = "no_second_look"    -> <<"look1", "count", "push", "check">>           \* no second look
      [] Mode = "look_before_count" -> <<"look1", "look2", "count", "push", "check">>  \* look, then count
      [] Mode = "refuse_after_push" -> <<"look1", "count", "push", "check", "look2">>  \* look after the push
      [] OTHER                      -> <<"look1", "count", "look2", "push", "check">>  \* the code's order
Finish == Len(Steps) + 1   \* a spawn past its last step is done

VARIABLES  \* the state
    draining,       \* `Shared::draining` of shard k: it takes no new task, and stops once none is live
    stopped,        \* `Shared::stopped`: the shard stopped and runs nothing more
    load,           \* `Shared::load`: the tasks counted on the shard (made, not done, not dropped)
    q,              \* the tasks queued on shard k (its inbox and its own queues): they go with the shard
    hq,             \* MUTATION bound only: the tasks queued on each thread's home instead
    sp,             \* each spawner's next step: an index into Steps (Finish: done)
    status,         \* each task: "none" "made" "queued" "polling" "ran" "dropped"
    answer,         \* what each spawn answered: "none" (not yet), "ok", "refused"
    polls,          \* the polls each task has begun
    homeOf,         \* MUTATION bound only: the thread each task is bound to (the runner as it was made)
    runner,         \* the thread that runs shard k now
    wpc,            \* shard k's loop: "loop", "look" (it read its first load), "poll", "exited"
    cur,            \* the task whose poll the loop is in (0: none)
    old,            \* the thread shard k was last taken from (0: none yet)
    oldTask,        \* the task whose poll that thread was in when the shard was taken (0: none)
    opc,            \* that thread: "none", "poll" (still in the poll), "push", "check", "free"
    handoffs,       \* the handoffs made so far
    runs,           \* (record) every poll begun: by which thread, on which shard, who ran k then
    lateRefused,    \* (record) a spawn was refused at its second look, after its task was counted
    waitingHandoff, \* (record) a handoff came while a placed task waited in shard k's queue
    drainedRun      \* (record) a task ran to its end while the shard was draining

shardVars == <<draining, stopped, load, q, hq>>                       \* what the shard holds
taskVars == <<status, polls, homeOf>>                                 \* what each task is
loopVars == <<runner, wpc, cur, handoffs>>                            \* who runs the loop, and where it is
oldVars == <<old, oldTask, opc>>                                      \* the thread the shard was taken from
recVars == <<runs, lateRefused, waitingHandoff, drainedRun>>          \* what the run records
spawnVars == <<sp, answer>>                                           \* where the spawns are
vars == <<shardVars, taskVars, loopVars, oldVars, recVars, spawnVars>> \* every variable

(***************************************************************************)
(* Helpers.                                                                *)
(***************************************************************************)

\* Spawner t is at step s.
At(t, s) == sp[t] <= Len(Steps) /\ Steps[sp[t]] = s  \* spawner t is at step s

\* Whether task u still counts in `load`: in the code a task counts until it completes or is
\* dropped (`Drop for Counted`).
\* MUTATION uncount_at_poll: the count goes as the task is taken for its first poll.
Counts(u) == Mode # "uncount_at_poll" \/ polls[u] = 0  \* u still counts in load

\* How many of the tasks in set S still count in `load`.
CountIn(S) == Cardinality({u \in S : Counts(u)})  \* how many of S count

\* The tasks shard k's loop may take: its queue, and (MUTATION bound) its runner's home's.
Ready == q \cup hq[runner]  \* what the loop may take

\* Where scheduling task u puts it: shard k's queue, which goes with the shard.
\* MUTATION bound: the queue of the home of the thread u is bound to, which stays with that thread.
QAdd(u) == IF Mode = "bound" THEN q ELSE q \cup {u}                              \* shard k's queue
HAdd(u) == IF Mode = "bound" THEN [hq EXCEPT ![homeOf[u]] = @ \cup {u}] ELSE hq  \* or the home's

\* Everything shard k queues is dropped unrun: each runnable, and with it its future and count.
DropQ ==  \* the queue dropped unrun
    /\ q' = {}                                                          \* the queue is emptied
    /\ status' = [u \in Tasks |-> IF u \in q THEN "dropped" ELSE status[u]]  \* each task in it dropped
    /\ load' = load - CountIn(q)                                        \* and no longer counted

\* Spawner t moves to its next step; after its last one the spawn answers Ok.
Advance(t) ==  \* the next step, Ok after the last
    /\ sp' = [sp EXCEPT ![t] = @ + 1]                                   \* the next step
    /\ answer' = IF sp[t] = Len(Steps)                                  \* that was the last one:
                 THEN [answer EXCEPT ![t] = "ok"]                       \* the spawn answers Ok
                 ELSE answer                                            \* otherwise not yet

\* The spawn of t refuses with InvalidInput and is done.
Refuse(t) ==  \* the spawn refuses
    /\ sp' = [sp EXCEPT ![t] = Finish]                                  \* no step is left
    /\ answer' = [answer EXCEPT ![t] = "refused"]                       \* it answers InvalidInput

(***************************************************************************)
(* A spawner (`spawn_on_worker`).                                          *)
(***************************************************************************)

\* The first look (`worker_shard`: `takes_tasks`): a draining shard is refused, nothing made.
Look1(t) ==  \* the first look
    /\ At(t, "look1")                                                   \* spawner t is here
    /\ IF draining THEN Refuse(t) ELSE Advance(t)                       \* refuse, or go on
    /\ UNCHANGED <<shardVars, taskVars, loopVars, oldVars, recVars>>    \* nothing else changes

\* The task is made: `Counted::new` counts it in the shard's load.
Count(t) ==  \* the count
    /\ At(t, "count")                                                   \* spawner t is here
    /\ load' = load + 1                                                 \* counted on shard k
    /\ status' = [status EXCEPT ![t] = "made"]                          \* the task exists, not scheduled
    /\ homeOf' = IF Mode = "bound"                                      \* MUTATION bound:
                 THEN [homeOf EXCEPT ![t] = runner]                     \* bound to k's runner now
                 ELSE homeOf                                            \* otherwise unused
    /\ Advance(t)                                                       \* the next step
    /\ UNCHANGED <<draining, stopped, q, hq, polls, loopVars, oldVars, recVars>>  \* nothing else

\* The SeqCst fence, then the second look at `draining`: up, the task is dropped unrun (its count
\* with it) and the spawn refuses.
Look2(t) ==  \* the fence and the second look
    /\ At(t, "look2")                                                   \* spawner t is here
    /\ IF draining                                                      \* the shard drains now:
          THEN /\ Refuse(t)                                             \* the spawn refuses
               /\ lateRefused' = (lateRefused \/ status[t] # "none")    \* (record) after its count
               /\ IF status[t] = "made"                                 \* made and not scheduled:
                     THEN /\ status' = [status EXCEPT ![t] = "dropped"] \* dropped here, unrun
                          /\ load' = load - 1                           \* and no longer counted
                     ELSE UNCHANGED <<status, load>>                    \* not made, or scheduled already
          ELSE /\ Advance(t)                                            \* otherwise it goes on
               /\ UNCHANGED <<status, load, lateRefused>>               \* and nothing more changes
    /\ UNCHANGED <<draining, stopped, q, hq, polls, homeOf, loopVars, oldVars,  \* these keep their values:
                   runs, waitingHandoff, drainedRun>>                   \* every variable not set above

\* The schedule (`runnable.schedule()`, `push_remote`'s push): the task goes on shard k's inbox.
Push(t) ==  \* the push
    /\ At(t, "push")                                                    \* spawner t is here
    /\ status' = [status EXCEPT ![t] = "queued"]                        \* waiting to be polled
    /\ q' = QAdd(t)                                                     \* on shard k's queue
    /\ hq' = HAdd(t)                                                    \* (MUTATION bound: its home's)
    /\ Advance(t)                                                       \* the next step
    /\ UNCHANGED <<draining, stopped, load, polls, homeOf, loopVars, oldVars, recVars>>  \* nothing else

\* `push_remote`'s look at `stopped`: a stopped shard's inbox is dropped (`drop_stopped`).
Check(t) ==  \* the look at stopped
    /\ At(t, "check")                                                   \* spawner t is here
    /\ IF stopped THEN DropQ ELSE UNCHANGED <<q, status, load>>         \* stopped: drop what it queues
    /\ Advance(t)                                                       \* the next step
    /\ UNCHANGED <<draining, stopped, hq, polls, homeOf, loopVars, oldVars, recVars>>  \* nothing else

\* Every step of spawner t.
Spawn(t) == Look1(t) \/ Count(t) \/ Look2(t) \/ Push(t) \/ Check(t)  \* every step of spawner t

(***************************************************************************)
(* The stopper (`Shared::drain`).                                          *)
(***************************************************************************)

\* `draining` goes up: the shard takes no new task and stops once its live tasks are done.
Drain ==  \* the drain
    /\ ~draining                                                        \* not drained yet
    /\ draining' = TRUE                                                 \* it drains from now on
    /\ UNCHANGED <<stopped, load, q, hq, taskVars, loopVars, oldVars, recVars, spawnVars>>  \* nothing else

(***************************************************************************)
(* Shard k's loop, run by whichever thread runs the shard (`run_worker`).  *)
(***************************************************************************)

\* The loop takes a queued task and begins its poll, on shard k, by the thread that runs it.
Take ==  \* the loop takes a task
    /\ wpc = "loop"                                                     \* the loop is between polls
    /\ \E u \in Ready :                                                 \* some queued task u
          /\ q' = q \ {u}                                               \* off the queue
          /\ hq' = [hq EXCEPT ![runner] = @ \ {u}]                      \* (or off the runner's home)
          /\ status' = [status EXCEPT ![u] = "polling"]                 \* its poll begins
          /\ polls' = [polls EXCEPT ![u] = @ + 1]                       \* one poll more
          /\ load' = IF Mode = "uncount_at_poll" /\ polls[u] = 0        \* MUTATION uncount_at_poll:
                     THEN load - 1                                      \* the count goes now
                     ELSE load                                          \* in the code it stays
          /\ cur' = u                                                   \* the loop polls u
    /\ wpc' = "poll"                                                    \* the loop is in the poll
    /\ runs' = runs \cup {[thread |-> runner, on |-> "k", runner |-> runner]}  \* (record) where it ran
    /\ UNCHANGED <<draining, stopped, homeOf, runner, handoffs, oldVars,  \* these keep their values:
                   lateRefused, waitingHandoff, drainedRun, spawnVars>> \* every variable not set above

\* `may_exit`'s first load: `draining`, found up (a look that finds it down changes nothing).
\* MUTATION load_first: the loop reads `load` first, and finds it zero.
Look ==  \* may_exit, first load
    /\ wpc = "loop"                                                     \* the loop is between polls
    /\ IF Mode = "load_first" THEN load = 0 ELSE draining               \* what the first load finds
    /\ wpc' = "look"                                                    \* the second load comes next
    /\ UNCHANGED <<shardVars, taskVars, runner, cur, handoffs, oldVars, recVars, spawnVars>>  \* nothing else

\* `may_exit`'s second load: `load` at zero, and the loop ends; the shard is dropped
\* (`Drop for Shard`): it is stopped, and what it queues is dropped unrun. Otherwise the loop goes on.
LookLoad ==  \* may_exit, second load
    /\ wpc = "look"                                                     \* after the first load
    /\ IF (IF Mode = "load_first" THEN draining ELSE load = 0)          \* what the second load finds:
          THEN /\ wpc' = "exited"                                       \* may exit: the loop ends
               /\ stopped' = TRUE                                       \* the shard stops
               /\ DropQ                                                 \* and drops what it queues
          ELSE /\ wpc' = "loop"                                         \* may not: back to the loop
               /\ UNCHANGED <<stopped, q, status, load>>                \* and nothing more changes
    /\ UNCHANGED <<draining, hq, polls, homeOf, runner, cur, handoffs, oldVars,  \* these keep their values:
                   recVars, spawnVars>>                                 \* every variable not set above

\* The poll of `cur` ends on the runner: the task completes (its count goes, and a draining
\* shard's `last_task_done` would wake the loop), or it is Pending and woken, and the runner puts
\* it back on shard k's queue.
PollEnd ==  \* a poll ends on the runner
    /\ wpc = "poll"                                                     \* the loop is in a poll
    /\ \/ /\ status' = [status EXCEPT ![cur] = "ran"]                   \* completed: it ran
          /\ load' = load - CountIn({cur})                              \* its count goes
          /\ drainedRun' = (drainedRun \/ draining)                     \* (record) during a drain
          /\ UNCHANGED <<q, hq>>                                        \* nothing is queued
       \/ /\ polls[cur] < MaxPolls                                      \* or, a poll before its last,
          /\ status' = [status EXCEPT ![cur] = "queued"]                \* Pending and woken
          /\ q' = QAdd(cur)                                             \* back on shard k's queue
          /\ hq' = HAdd(cur)                                            \* (MUTATION bound: its home's)
          /\ UNCHANGED <<load, drainedRun>>                             \* still counted
    /\ wpc' = "loop"                                                    \* the loop goes on
    /\ cur' = 0                                                         \* in no poll
    /\ UNCHANGED <<draining, stopped, polls, homeOf, runner, handoffs, oldVars,  \* these keep their values:
                   runs, lateRefused, waitingHandoff, spawnVars>>       \* every variable not set above

(***************************************************************************)
(* The monitor's handoff (`take` in src/rt/handoff/monitor.rs).            *)
(***************************************************************************)

\* While the runner is in a poll, the monitor takes shard k and gives it to the other thread,
\* which runs the loop from its start; the old runner stays in that one poll.
Handoff ==  \* the monitor hands shard k on
    /\ wpc = "poll"                                                     \* the runner is in a poll
    /\ handoffs < MaxHandoffs                                           \* the monitor may hand on
    /\ opc \in {"none", "free"}                                         \* the other thread is a spare
    /\ old' = runner                                                    \* the runner loses the shard
    /\ oldTask' = cur                                                   \* and keeps the poll it is in
    /\ opc' = "poll"                                                    \* which it still runs
    /\ runner' = Other(runner)                                          \* the spare runs shard k now
    /\ wpc' = "loop"                                                    \* from the loop's start
    /\ cur' = 0                                                         \* in no poll
    /\ handoffs' = handoffs + 1                                         \* one handoff more
    /\ waitingHandoff' = (waitingHandoff \/ q # {})                     \* (record) with a task waiting
    /\ UNCHANGED <<shardVars, taskVars, runs, lateRefused, drainedRun, spawnVars>>  \* nothing else

(***************************************************************************)
(* The thread shard k was taken from: it finishes its poll, detached.      *)
(***************************************************************************)

\* The old runner's poll ends: the task completes, or it is Pending and woken; the waker finds
\* no shard on this thread (it is detached) and takes the remote path.
OldEnd ==  \* the old runner's poll ends
    /\ opc = "poll"                                                     \* the old runner is in the poll
    /\ \/ /\ status' = [status EXCEPT ![oldTask] = "ran"]               \* completed: it ran
          /\ load' = load - CountIn({oldTask})                          \* its count goes
          /\ drainedRun' = (drainedRun \/ draining)                     \* (record) during a drain
          /\ opc' = "free"                                              \* the thread is free
          /\ oldTask' = 0                                               \* in no poll
       \/ /\ polls[oldTask] < MaxPolls                                  \* or, a poll before its last,
          /\ status' = [status EXCEPT ![oldTask] = "made"]              \* Pending and woken
          /\ opc' = "push"                                              \* it is pushed back next
          /\ UNCHANGED <<load, drainedRun, oldTask>>                    \* still counted
    /\ UNCHANGED <<draining, stopped, q, hq, polls, homeOf, loopVars, old,  \* these keep their values:
                   runs, lateRefused, waitingHandoff, spawnVars>>       \* every variable not set above

\* The waker's `push_remote`, its push: the task goes back on shard k's inbox, wherever k runs.
OldPush ==  \* its waker pushes the task back
    /\ opc = "push"                                                     \* a woken task to push
    /\ status' = [status EXCEPT ![oldTask] = "queued"]                  \* waiting to be polled
    /\ q' = QAdd(oldTask)                                               \* on shard k's queue
    /\ hq' = HAdd(oldTask)                                              \* (MUTATION bound: its home's)
    /\ opc' = "check"                                                   \* the look at `stopped` next
    /\ UNCHANGED <<draining, stopped, load, polls, homeOf, loopVars, old, oldTask,  \* these keep their values:
                   recVars, spawnVars>>                                 \* every variable not set above

\* `push_remote`'s look at `stopped`: a stopped shard's inbox is dropped.
OldCheck ==  \* and looks at stopped
    /\ opc = "check"                                                    \* after the push
    /\ IF stopped THEN DropQ ELSE UNCHANGED <<q, status, load>>         \* stopped: drop what it queues
    /\ opc' = "free"                                                    \* the thread is free
    /\ oldTask' = 0                                                     \* in no poll
    /\ UNCHANGED <<draining, stopped, hq, polls, homeOf, loopVars, old, recVars, spawnVars>>  \* nothing else

\* MUTATION bound: the thread the shard was taken from runs the tasks of its home on a shard of
\* its own (`finish_home`), to their end, away from worker k. Never enabled otherwise.
Own ==  \* (bound) the old thread runs its home
    /\ opc = "free"                                                     \* the old thread is free
    /\ \E u \in hq[old] :                                               \* a task of its home
          /\ hq' = [hq EXCEPT ![old] = @ \ {u}]                         \* off the home's queue
          /\ status' = [status EXCEPT ![u] = "ran"]                     \* it runs to its end
          /\ polls' = [polls EXCEPT ![u] = @ + 1]                       \* a poll begun
          /\ load' = load - CountIn({u})                                \* its count goes
    /\ runs' = runs \cup {[thread |-> old, on |-> "own", runner |-> runner]}  \* (record) where it ran
    /\ UNCHANGED <<draining, stopped, q, homeOf, loopVars, oldVars,  \* these keep their values:
                   lateRefused, waitingHandoff, drainedRun, spawnVars>> \* every variable not set above

(***************************************************************************)
(* The system.                                                             *)
(***************************************************************************)

\* Every step of shard k's loop.
Loop == Take \/ Look \/ LookLoad \/ PollEnd  \* the loop's steps

\* Every step of the thread the shard was taken from.
OldThread == OldEnd \/ OldPush \/ OldCheck \/ Own  \* the old thread's steps

Init ==  \* the start
    /\ draining = FALSE                     \* the shard takes tasks
    /\ stopped = FALSE                      \* and runs
    /\ load = 0                             \* with no task counted
    /\ q = {}                               \* and none queued
    /\ hq = [th \in Threads |-> {}]         \* no home holds a task
    /\ sp = [t \in Tasks |-> 1]             \* every spawner is at its first step
    /\ status = [t \in Tasks |-> "none"]    \* no task is made
    /\ answer = [t \in Tasks |-> "none"]    \* no spawn answered
    /\ polls = [t \in Tasks |-> 0]          \* no poll began
    /\ homeOf = [t \in Tasks |-> 1]         \* (bound to nobody yet: set at the count)
    /\ runner = 1                           \* thread 1 runs shard k
    /\ wpc = "loop"                         \* its loop is between polls
    /\ cur = 0                              \* in no poll
    /\ old = 0                              \* no thread lost the shard
    /\ oldTask = 0                          \* so none is in a poll of it
    /\ opc = "none"                         \* and thread 2 is a spare
    /\ handoffs = 0                         \* no handoff yet
    /\ runs = {}                            \* no poll began
    /\ lateRefused = FALSE                  \* nothing refused late
    /\ waitingHandoff = FALSE               \* no handoff with a task waiting
    /\ drainedRun = FALSE                   \* no task ran during a drain

Next ==  \* every step
    \/ \E t \in Tasks : Spawn(t)            \* a spawner's step
    \/ Drain                                \* the stopper drains worker k
    \/ Loop                                 \* shard k's loop
    \/ Handoff                              \* the monitor hands shard k on
    \/ OldThread                            \* the thread shard k was taken from

\* The spawners, the loop and the old thread are weakly fair; the loop takes a task it can take
\* infinitely often (strong fairness: it never only looks whether it may exit while tasks wait).
\* The stopper and the monitor need not act.
Spec ==  \* the behaviours, with fairness
    /\ Init                                 \* from the start
    /\ [][Next]_vars                        \* every step is one of these
    /\ \A t \in Tasks : WF_vars(Spawn(t))   \* every spawn finishes
    /\ WF_vars(Loop)                        \* the loop keeps going
    /\ SF_vars(Take)                        \* and takes what waits
    /\ WF_vars(OldThread)                   \* the old runner finishes its poll

(***************************************************************************)
(* What must hold.                                                         *)
(***************************************************************************)

\* A task whose spawn answered Ok is never dropped unrun: it ran, is in a poll, or still waits.
NoSilentLoss == \A t \in Tasks : answer[t] = "ok" => status[t] # "dropped"  \* answered Ok, never dropped unrun

\* A task whose spawn was refused is never polled.
RefusedNeverRuns == \A t \in Tasks : answer[t] = "refused" => polls[t] = 0  \* refused, never polled

\* Every poll of a placed task begins on shard k, by the thread that runs shard k at that moment,
\* so a handoff never takes the placement away.
RunsOnItsWorker == \A r \in runs : r.on = "k" /\ r.thread = r.runner  \* polled on shard k by its runner

\* A stopped shard queues nothing: no task is pushed to it once it stopped, so on this path
\* `push_remote`'s look at `stopped` never drops a task.
StoppedQueuesNothing == stopped => q = {}  \* a stopped shard queues nothing

\* Some task whose spawn answered Ok has not run to its end yet.
OkWaits == \E t \in Tasks : answer[t] = "ok" /\ status[t] # "ran"  \* an Ok task not run yet

\* Every task whose spawn answered Ok runs to its end: whenever one waits, a moment comes when
\* none does (an answer and a completion never change back, so this is each task's own promise).
EveryOkRuns == OkWaits ~> ~OkWaits  \* each Ok task runs

\* Witnesses: each claims an interesting case never happens, and must be proved wrong.
NoLateRefusal == ~lateRefused                       \* no spawn is refused after its count
NoHandoffWhileWaiting == ~waitingHandoff            \* no handoff while a placed task waits
NoDrainedRun == ~(drainedRun /\ stopped)            \* no drain runs a task to its end, then stops
=============================================================================

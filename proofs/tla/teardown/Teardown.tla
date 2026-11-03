------------------------------ MODULE Teardown ------------------------------
(***************************************************************************)
(* A runtime's drop while its monitor may still take a shard and hand it   *)
(* on, and the end of every task as the runtime closes (src/rt/runtime.rs: *)
(* `Runtime::drop`, `stop_and_join`, `join`, `join_detached`;              *)
(* src/rt/workers.rs: `Workers::stop_all`, `take_monitor`, `take_threads`, *)
(* `runner`, `run_shard`, `finish_home`, `finish_taken`;                   *)
(* src/rt/task/list.rs: the task lists; src/rt/shard/going.rs: `Shard::go`;*)
(* src/rt/handoff/home.rs: `Home::cancel_all`; src/rt/handoff/monitor.rs:  *)
(* `run`, `take`, `give`, `let_go`, `Monitor::close`, `wait_for_shard`,    *)
(* `home_done`, `Spare::drop`).                                            *)
(*                                                                          *)
(* Who acts. The dropper D, on a thread of the user's or (`OnRunner`) in a *)
(* task on W, one of the runtime's own runner threads. The monitor M,      *)
(* which may take shard S from its runner T1 while T1 is blocked in one    *)
(* poll, and hands S to a waiting spare or to a runner T2 it starts. T1    *)
(* itself, whose poll returns at some moment, or (`Stuck`) maybe never. A  *)
(* spare P, which waits for a shard and leaves once it sees the close. T2, *)
(* once started. The clock C, whose deadline (the shutdown's timeout, or   *)
(* the default bound) passes some time after the close. X, outside, which  *)
(* may wake T1's bound task once (its push lands a moment after the wake), *)
(* or never.                                                               *)
(*                                                                          *)
(* The tasks. A, a task of S whose future may go to any thread: listed in *)
(* S's list once its first poll returns pending, and before that only a    *)
(* runnable in S's queue. B1, bound to T1 (its future may be polled and    *)
(* dropped on T1 only), listed in T1's home. B2, bound to T2, whose        *)
(* runnable waits in S's queue as S stops.                                 *)
(*                                                                          *)
(* What can happen. The drop closes the monitor, stops every shard, joins  *)
(* the monitor's thread, then takes the list of threads to join: every one *)
(* started but T1 if it is detached (its shard taken while it is still in  *)
(* its poll), and never its own thread. It joins each, then waits for T1,  *)
(* if it was detached, only until the deadline, and says so if T1 is still *)
(* in that poll then. The monitor's loop looks at the close at its head    *)
(* only: a pass under way may still take S and hand it on, and its        *)
(* hand-off may pop a spare that saw the close and left. The runner of S   *)
(* as S stops drops what S's queues hold and walks S's list, cancelling    *)
(* each task there, on its own thread, before it lets go of S; S's last    *)
(* holder does the same as S goes. Each runner, as it is done with its     *)
(* home at the close, walks the home's list and cancels each bound task on *)
(* its own thread: one idle has its future dropped at once; one whose wake *)
(* is on its way is waited for until its runnable lands, or the deadline,  *)
(* after which it is left, and said.                                       *)
(*                                                                          *)
(* What must never happen. The drop waits without bound on T1 while T1 is *)
(* blocked in a poll whose shard was taken; the drop returns with a runner *)
(* it started still running; the drop joins its own thread; a shard's last *)
(* reference goes on a thread outside its runtime; the drop returns before *)
(* its deadline with S still alive; it returns with T1 still in its taken  *)
(* poll and says nothing; a runner goes past its home's walk with a bound  *)
(* task live; a shard's walk leaves a task of the shard live; a bound task *)
(* is dropped on a thread that is not its home's.                          *)
(*                                                                          *)
(* What is abstract. One take at most, of S, by a pass that may straddle  *)
(* the close; its claim, fence and compare-and-swap are one step          *)
(* (tla/handoff/Handoff.tla checks them). A shard's run until it stops is  *)
(* one step, A's first poll aside. A spare is P: T1 and T2 waiting as      *)
(* spares get no gift (the one gift goes to P or T2) and leave at the      *)
(* close. Thread starts never fail. A runnable is "queued" wherever it     *)
(* waits for its thread (a shard's queue, the home's inbox). A worker a    *)
(* task starts as the drop runs is tla/teardown/Grow.tla's.                *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC                                            \* numbers, set sizes, TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    OnRunner,                                                                \* TRUE: the drop runs in a task on W, one of the runtime's runner threads
    Stuck,                                                                   \* TRUE: T1's poll may never return
    Mutation                                                                 \* "none", or the name of the one rule this run takes away

\* The dropper's own thread: a user's thread "U", or runner "W".
Me == IF OnRunner THEN "W" ELSE "U"                                          \* who runs the drop

\* What a bound task may end as: ran to its end, cancelled on its thread, or left (a wake still on its way at the deadline) and said.
Settled == {"done", "cancelled", "left"}                                     \* the ends of a bound task

(* --algorithm Teardown                                                      \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    closed = FALSE,                                                          \* `Monitor::close` ran its store
    stopped = FALSE,                                                         \* `stop_all` stopped every shard: a runner's loop ends at its next look
    late = FALSE,                                                            \* the drop's deadline passed (the timeout given, or the default bound)
    taken = FALSE,                                                           \* the monitor took S from T1
    lateTake = FALSE,                                                        \* (record) that take came after the close
    detached = FALSE,                                                        \* T1's life says it is out on a taken poll (`Life::detach`)
    returns = FALSE,                                                         \* (with Stuck) T1's poll returns from now on
    refs = 1,                                                                \* strong references to S: T1's to begin with
    lastDrop = "none",                                                       \* where S's last reference went: "none", "runtime" (inside it), "outside"
    gift = "none",                                                           \* who holds the gift of S: "none", "M", "T2", "P" (stored in P's spare), "gone"
    queue = {},                                                              \* the spares the monitor's queue holds (`Monitor::spares`)
    pAlive = TRUE,                                                           \* P's thread still holds its own reference to its spare
    unparked = FALSE,                                                        \* P was unparked and has not looked since
    started = {"T1", "P", "M"} \cup (IF OnRunner THEN {"W"} ELSE {}),        \* the threads the runtime started
    list = {},                                                               \* the threads the drop took to join without a bound
    detachedAtSnap = FALSE,                                                  \* T1 was detached when the list was taken: waited for until the deadline
    joining = "none",                                                        \* the thread the drop waits for now
    joinBound = FALSE,                                                       \* that wait ends at the deadline
    toldLeft = FALSE,                                                        \* the drop said it left T1 in its taken poll
    dropDone = FALSE,                                                        \* `Runtime::drop` returned
    a = "queued",                                                            \* A: "queued" (never polled: a runnable in S's queue), "idle" (pending), "cancelled"
    aListed = FALSE,                                                         \* A is in S's list
    walkedS = FALSE,                                                         \* S's list was walked: by its runner as S stopped, or as S went
    b1 = "live",                                                             \* B1: "live" (idle), "woken" (its wake on its way), "queued", "done", "cancelled", "left"
    dropB1 = "none",                                                         \* the thread that dropped B1's future, "none" before
    toldTasks = FALSE,                                                       \* T1 said a bound task was left
    b2 = "none",                                                             \* B2, once T2 runs S: "none", "queued" (in S's queue), "cancelled"
    dropB2 = "none",                                                         \* the thread that dropped B2's future, "none" before
    deadGift = FALSE;                                                        \* (record) a gift was handed to a spare whose thread had left

define                                                                       \* names the steps and properties use:
    \* Whether thread t has ended: W runs the drop, so it ends only after the drop.
    Finished(t) == IF t = "W" \/ t = "U" THEN FALSE ELSE pc[t] = "Done"      \* a process at its end
    \* The drop never waits without a bound on T1 while T1 is blocked in a poll whose shard was taken.
    JoinsNoDetached == ~(joining = "T1" /\ ~joinBound /\ detached /\ pc["T1"] = "T1_poll")  \* never such a wait
    \* When the drop returns, every thread the runtime started has ended, or was detached at the list, or is the dropper's own.
    StartedJoined == dropDone =>                                             \* once the drop returned:
        \A t \in started : Finished(t) \/ t = Me \/ (t = "T1" /\ detachedAtSnap)  \* each one is accounted for
    \* The drop never joins its own thread.
    NoSelfJoin == joining # Me                                               \* never itself
    \* A shard's last reference never goes on a thread outside its runtime.
    LastDropInRuntime == lastDrop # "outside"                                \* always inside
    \* The drop returns.
    DropReturns == <>dropDone                                                \* in the end
    \* The drop returns, unless T1 stays blocked in a poll the monitor never took (a drop waits for a
    \* runner's poll, as it always has; a detached one it waits for only until the deadline).
    DropReturnsOrBlocked == <>(dropDone \/ (Finished("M") /\ ~taken /\ pc["T1"] = "T1_poll"))  \* returns, or T1 never taken
    \* The drop returns with S still alive only once its deadline passed.
    DropDropsTasks == dropDone => refs = 0 \/ late                           \* S gone, or the deadline passed
    \* The drop never returns with T1 still in its taken poll and says nothing.
    LeftReported == (dropDone /\ taken /\ pc["T1"] = "T1_poll") => toldLeft  \* said, if so
    \* Once S's list was walked, A is cancelled: no task of S is left live.
    ShardSettled == walkedS => a = "cancelled"                               \* every task of the shard
    \* T1 never goes past its home's walk with its bound task live.
    HomeSettled == pc["T1"] \in {"T1_drop", "T1_spare", "Done"} => b1 \in Settled  \* every task of the home
    \* A bound task left is always said.
    LeftSaid == b1 = "left" => toldTasks                                     \* never silently
    \* Each bound task's future is dropped on its home's thread only.
    DroppedOnHome == dropB1 \in {"none", "T1"} /\ dropB2 \in {"none", "T2"}  \* T1's by T1, T2's by T2
    \* Once the close came, every task is ended in the end.
    AllEnd == closed ~> (a = "cancelled" /\ b1 \in Settled /\ b2 # "queued") \* none live for good
    \* Witness: the monitor takes S after the close.
    NoLateTake == ~lateTake                                                  \* claims to: never
    \* Witness: a gift is handed to a spare that had left.
    NoDeadGift == ~deadGift                                                  \* claims to: never
    \* Witness: the drop says it left T1 in its taken poll.
    NoToldLeft == ~toldLeft                                                  \* claims to: never
    \* Witness: a home's walk finds a wake on its way, and waits for it.
    NoWokenAtWalk == ~(pc["T1"] = "T1_cancel" /\ b1 = "woken")               \* claims to: never
end define;                                                                  \* end of the names

\* The thread that runs S, as S stops, or S's last holder, as S goes: it drops what S's queues hold
\* (A never polled, B2's runnable) and walks S's list, cancelling each task in it, all on this thread.
macro EndShard(who) begin                                                    \* `drop_queued`, then S's list's walk
    if a = "queued" \/ (a = "idle" /\ aListed) then                          \* in S's queue, or in S's list:
        a := "cancelled";                                                    \* its future dropped here
    end if;                                                                  \* MUTATION unlisted: an idle A left out of the list stays live
    if b2 = "queued" then                                                    \* B2's runnable in S's queue:
        b2 := "cancelled";                                                   \* its future dropped here
        dropB2 := who;                                                       \* on this thread
    end if;                                                                  \* S's queues are empty
    walkedS := TRUE;                                                         \* the walk is done
end macro;                                                                   \* end of the macro

\* One reference to S less, let go of inside the runtime unless `outside`; the last one drops S, and
\* S's going ends what is still on it, on the thread that lets go.
macro LetGo(outside, who) begin                                              \* a holder lets go of S
    refs := refs - 1;                                                        \* one fewer
    if refs = 0 then                                                         \* the last:
        lastDrop := IF outside THEN "outside" ELSE "runtime";                \* where S drops
        EndShard(who);                                                       \* `Shard::go`
    end if;                                                                  \* not the last: S lives on
end macro;                                                                   \* end of the macro

\* A runner of S polls A, never polled yet: its future returns pending, and is listed in S's list.
macro PollA() begin                                                          \* A's first poll
    if a = "queued" then                                                     \* never polled yet
        a := "idle";                                                         \* pending
        aListed := Mutation # "unlisted";                                    \* MUTATION unlisted: not linked
    end if;                                                                  \* else nothing to poll
end macro;                                                                   \* end of the macro

\* The drop (`Runtime::drop`, `stop_and_join`).
fair process dropper = "D"                                                   \* weakly fair: the drop goes on
begin                                                                        \* the drop's steps:
D_close:                                                                     \* `Monitor::close`: the store, then the pops
    closed := TRUE;                                                          \* the monitor and spares stop
    if "P" \in queue then unparked := TRUE; end if;                          \* a spare popped is unparked
    queue := {};                                                             \* the popped spares hold no gift
D_stop:                                                                      \* the shards stopped
    stopped := TRUE;                                                         \* each runner's loop ends at its next look
D_monitor:                                                                   \* `take_monitor`, joined first
    \* MUTATION snapshot_first: the list is taken while the monitor may still take and start, as before the fix.
    if Mutation # "snapshot_first" then await Finished("M"); end if;         \* no take or start comes after
D_snap:                                                                      \* `take_threads`, then the filter of `join`
    list := {t \in started \ (IF Mutation = "snapshot_first" THEN {} ELSE {"M"}) :  \* every thread started,
               /\ (t = "T1" => ~detached \/ Mutation = "no_detached_filter") \* MUTATION no_detached_filter: T1 even detached
               /\ (t # Me \/ Mutation = "self_join")};                       \* MUTATION self_join: its own thread too
    detachedAtSnap := detached /\ Mutation # "no_detached_filter";           \* T1, detached, is waited for apart
D_join:                                                                      \* the joins, one at a time
    while list # {} do                                                       \* while one is left
        with t \in list do joining := t; end with;                           \* the next to join
D_wait:                                                                      \* `JoinHandle::join`
        await Finished(joining);                                             \* until it ended
        list := list \ {joining};                                            \* joined
        joining := "none";                                                   \* waiting for none
    end while;                                                               \* every one joined
D_detached:                                                                  \* `join_detached`: T1, detached at the list
    if detachedAtSnap then                                                   \* waited for until the deadline
        joining := "T1";                                                     \* T1 next
        joinBound := TRUE;                                                   \* a wait that ends at the deadline
D_wait_detached:                                                             \* until T1 ended, or the deadline
        \* MUTATION detached_unbounded: the wait for T1 has no deadline: T1 stuck in its poll holds the drop for good.
        await Finished("T1") \/ (late /\ Mutation # "detached_unbounded");   \* T1 done, or time is up
        if ~Finished("T1") /\ taken /\ pc["T1"] = "T1_poll" then             \* still in its taken poll:
            toldLeft := TRUE;                                                \* said once, at warn
        end if;                                                              \* else it ends on its own
        joining := "none";                                                   \* waiting for none
        joinBound := FALSE;                                                  \* that wait is over
    end if;                                                                  \* every wait done
D_done:                                                                      \* `finish_lingering` aside, the drop returns
    dropDone := TRUE;                                                        \* done
end process;                                                                 \* end of the drop

\* The monitor (`monitor::run`, `take`, `give`).
fair process monitor = "M"                                                   \* weakly fair: it ticks
begin                                                                        \* the monitor's steps:
M_head:                                                                      \* the loop's look at the close
    if closed then goto Done; end if;                                        \* closed: the monitor ends
M_take:                                                                      \* a pass's look at T1: maybe the take
    if ~taken /\ pc["T1"] = "T1_poll" then                                   \* T1 blocked, no take yet:
        either                                                               \* the poll ran long enough:
            taken := TRUE;                                                   \* S is the monitor's
            lateTake := closed;                                              \* (record) after the close
            detached := TRUE;                                                \* `old.life.detach()`
            refs := refs + 1;                                                \* the reference held since before the claim
            gift := "M";                                                     \* the gift, in the monitor's hands
        or skip;                                                             \* not yet
        end either;                                                          \* one of the two
    end if;                                                                  \* now the hand-off
M_give:                                                                      \* `give`, in the same pass
    if gift = "M" then                                                       \* a gift to hand on:
        if queue # {} then                                                   \* a spare waits:
            queue := queue \ {"P"};                                          \* popped
            deadGift := deadGift \/ ~pAlive;                                 \* (record) its thread had left
            if pAlive then                                                   \* it lives: its gift is stored, and it unparked
                gift := "P";                                                 \* P takes it at its next look
                unparked := TRUE;                                            \* `spare.thread.unpark()`
            else                                                             \* it left: the popped reference is the last
                gift := "gone";                                              \* `Spare::drop` drops the gift
                \* MUTATION dead_spare_drop: the popped spare is dropped plainly, outside the runtime.
                LetGo(Mutation = "dead_spare_drop", "M");                    \* fixed: through `let_go`, inside it
            end if;                                                          \* handed, or gone
        else                                                                 \* none waits: a runner is started
            started := started \cup {"T2"};                                  \* `spawn_runner`
            gift := "T2";                                                    \* T2 runs S
        end if;                                                              \* handed on
    end if;                                                                  \* the pass is over
    goto M_head;                                                             \* the next tick
end process;                                                                 \* end of the monitor

\* T1, S's first runner, blocked in one poll that returns at some moment (or, with Stuck, maybe never).
fair process t1 = "T1"                                                       \* weakly fair: it goes on when it can
begin                                                                        \* T1's steps:
T1_poll:                                                                     \* in the poll
    await ~Stuck \/ returns;                                                 \* it returns, if it ever does
T1_back:                                                                     \* `come_back`
    if taken then goto T1_home; end if;                                      \* taken: detached
T1_run:                                                                      \* back in: S runs
    PollA();                                                                 \* A's first poll, if it has not run
T1_stop:                                                                     \* until S stops
    await stopped;                                                           \* the loop's look at `stopped`
    \* MUTATION close_abandons: nothing is dropped or walked at the close, as before the task lists.
    if Mutation # "close_abandons" then                                      \* `drop_queued` and S's walk, on T1
        if b1 = "queued" then                                                \* B1's runnable in S's queue:
            b1 := "cancelled";                                               \* its future dropped here
            dropB1 := "T1";                                                  \* on its home's thread
        end if;                                                              \* S's queue holds none of T1's
        EndShard("T1");                                                      \* A cancelled here
    end if;                                                                  \* S is done with
T1_let:                                                                      \* `drop(shard)`
    LetGo(FALSE, "T1");                                                      \* inside the runtime
    if Mutation = "close_abandons" then goto T1_spare;                       \* (mutated) the home not walked
    else goto T1_walk;                                                       \* then the home's walk
    end if;                                                                  \* one of the two
T1_home:                                                                     \* `finish_home`: until its bound task ends, or the close
    either                                                                   \* one of the two:
        await b1 = "queued";                                                 \* woken: it runs on T1's own shard
        b1 := "done";                                                        \* and ends
    or                                                                       \* the close came first:
        await closed;                                                        \* `home_done` is ready
    end either;                                                              \* then the home's walk
    if Mutation = "close_abandons" then goto T1_drop; end if;                \* (mutated) the bound task left as it is
T1_walk:                                                                     \* `Home::cancel_all`, while the home's list holds a task
    while b1 \in {"live", "woken", "queued"} do                              \* not ended yet
T1_cancel:                                                                   \* the walk's look at B1
        await b1 # "woken" \/ late;                                          \* a wake on its way: its runnable lands, or time is up
        if b1 \in {"live", "queued"} then                                    \* idle (its future taken by the walk), or its runnable here:
            b1 := "cancelled";                                               \* its future dropped here
            dropB1 := "T1";                                                  \* on its home's thread
        elsif b1 = "woken" then                                              \* the deadline passed with its runnable still on its way:
            b1 := "left";                                                    \* left
            toldTasks := TRUE;                                               \* said once at warn, and counted
        end if;                                                              \* settled
    end while;                                                               \* the home's list is empty
    if ~taken then goto T1_spare; end if;                                    \* a worker that stopped: a spare now
T1_drop:                                                                     \* `drop(shard)` in `finish_taken`
    LetGo(FALSE, "T1");                                                      \* inside the runtime
T1_spare:                                                                    \* a spare until the close
    await closed;                                                            \* it leaves
end process;                                                                 \* end of T1

\* T2, a runner the monitor started for S.
fair process t2 = "T2"                                                       \* weakly fair once started
begin                                                                        \* T2's steps:
T2_start:                                                                    \* the thread starts, or never will
    await "T2" \in started \/ Finished("M");                                 \* started, or the monitor is gone
    if "T2" \notin started then goto Done; end if;                           \* never started
T2_run:                                                                      \* S runs: a task bound to T2 is woken
    PollA();                                                                 \* A's first poll, if it has not run
    b2 := "queued";                                                          \* B2's runnable waits in S's queue
T2_stop:                                                                     \* until S stops
    await stopped;                                                           \* the loop's look
    \* MUTATION drop_off_thread: T2 lets go of S without dropping S's queues or walking its list; S's last holder does it.
    if Mutation # "drop_off_thread" then                                     \* `drop_queued` and S's walk, on T2
        EndShard("T2");                                                      \* B2 and A ended here
    end if;                                                                  \* S is done with
T2_let:                                                                      \* `drop(shard)`
    gift := "gone";                                                          \* T2's run is over
    LetGo(FALSE, "T2");                                                      \* inside the runtime
T2_spare:                                                                    \* a spare until the close
    await closed;                                                            \* it leaves
end process;                                                                 \* end of T2

\* P, a spare (`wait_for_shard`).
fair process p = "P"                                                         \* weakly fair: it runs
begin                                                                        \* P's steps:
P_push:                                                                      \* it pushes itself on the queue
    queue := queue \cup {"P"};                                               \* waiting
    \* MUTATION park_first: it parks before its first look, so a push after the close's pops is never unparked.
    if Mutation = "park_first" then goto P_park; end if;                     \* (mutated) no look first
P_look:                                                                      \* its look: a gift, then the close
    if gift = "P" then goto P_run;                                           \* a gift: it runs it
    elsif closed then goto P_last;                                           \* closed: its last look
    end if;                                                                  \* else it parks
P_park:                                                                      \* `thread::park`, until unparked
    await unparked;                                                          \* by the close's pop, or a gift
    unparked := FALSE;                                                       \* taken
    goto P_look;                                                             \* it looks again
P_last:                                                                      \* `return me.take_gift()`
    if gift = "P" then goto P_run; end if;                                   \* a gift stored before the close: run
P_exit:                                                                      \* its thread ends: its own reference goes
    pAlive := FALSE;                                                         \* gone
    if gift = "P" /\ "P" \notin queue then                                   \* a gift stored after its last look, the queue's reference dropped:
        gift := "gone";                                                      \* `Spare::drop` here
        LetGo(FALSE, "P");                                                   \* on its thread, inside the runtime
    end if;                                                                  \* gone
    goto Done;                                                               \* ended
P_run:                                                                       \* the gift's shard runs until it stops
    await stopped;                                                           \* the loop's look
    EndShard("P");                                                           \* `drop_queued` and S's walk, on P
P_let:                                                                       \* `drop(shard)`
    gift := "gone";                                                          \* its run is over
    LetGo(FALSE, "P");                                                       \* inside the runtime
    goto P_push;                                                             \* a spare again
end process;                                                                 \* end of P

\* The clock: the drop's deadline passes some time after the close.
fair process clock = "C"                                                     \* weakly fair: time goes on
begin                                                                        \* one step:
C_late:                                                                      \* the deadline
    await closed;                                                            \* counted from the drop's start
    late := TRUE;                                                            \* passed
end process;                                                                 \* end of the clock

\* X, outside: wakes T1's bound task once, at any moment, or never; a wake begun lands.
fair process waker = "X"                                                     \* weakly fair: it acts once, maybe a no-op
begin                                                                        \* its steps:
X_wake:                                                                      \* a wake, or none
    either                                                                   \* one of the two:
        await b1 = "live";                                                   \* the task waits
        b1 := "woken";                                                       \* SCHEDULED set: its runnable in X's hands
    or                                                                       \* or
        goto Done;                                                           \* no wake comes
    end either;                                                              \* then the push
X_push:                                                                      \* `Home::push`
    if b1 = "woken" then b1 := "queued"; end if;                             \* the runnable lands in the home's inbox
end process;                                                                 \* end of X

\* E, what T1's poll waits on (with Stuck): it may let the poll return, or never (not fair).
process env = "E"                                                            \* may never act
begin                                                                        \* one step:
E_return:                                                                    \* the poll may return now
    returns := TRUE;                                                         \* from now on
end process;                                                                 \* end of E
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

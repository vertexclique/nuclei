------------------------------ MODULE Teardown ------------------------------
(***************************************************************************)
(* A runtime's drop while its monitor may still take a shard and hand it   *)
(* on (src/rt/runtime.rs: `Runtime::drop`, `stop_and_join`, `join`;        *)
(* src/rt/workers.rs: `Workers::stop_all`, `take_monitor`, `take_threads`, *)
(* `start_monitor`, `runner`, `run_shard`, `finish_home`; src/rt/handoff/  *)
(* monitor.rs: `run`, `take`, `give`, `let_go`, `Monitor::close`,          *)
(* `wait_for_shard`, `home_done`, `Spare::drop`).                          *)
(*                                                                          *)
(* Who acts. The dropper D, on a thread of the user's or (`OnRunner`) in a *)
(* task on W, one of the runtime's own runner threads. The monitor M,      *)
(* which may take shard S from its runner T1 while T1 is blocked in one    *)
(* poll, and hands S to a waiting spare or to a runner T2 it starts. T1    *)
(* itself, whose poll returns at some moment. A spare P, which waits for a *)
(* shard and leaves once it sees the close. T2, once started.              *)
(*                                                                          *)
(* What can happen. The drop closes the monitor (the closed flag, then the *)
(* spares waiting are popped and unparked), stops every shard, joins the   *)
(* monitor's thread, then takes the list of threads to join: every one     *)
(* started, but T1 if it is detached (its shard taken while it is still in *)
(* its poll, which the drop does not wait for), and never its own thread.  *)
(* It joins each, then returns. The monitor's loop looks at the close at   *)
(* its head only: a pass under way when the close comes may still take S   *)
(* and hand it on, and its hand-off may pop a spare that saw the close and *)
(* left, whose last reference then goes in that hand-off.                  *)
(*                                                                          *)
(* What must never happen. The drop waits on T1 while T1 is blocked in a   *)
(* poll whose shard was taken (it may never return); the drop returns with *)
(* a runner it started still running, not joined (it drops a shard's tasks *)
(* after the drop); the drop joins its own thread (a deadlock, or a panic  *)
(* on a failed join); a shard's last reference goes on a thread outside    *)
(* its runtime (what its tasks' handles leave would go to another runtime).*)
(* Two more are today's design, which the operator has to settle: the drop *)
(* returns while detached T1 still holds S, so S's tasks are dropped only  *)
(* when T1's poll returns; and T1's bound tasks are abandoned at the close.*)
(*                                                                          *)
(* What is abstract. One take at most, of S, by a pass that may straddle  *)
(* the close; its claim, fence and compare-and-swap are one step          *)
(* (tla/handoff/Handoff.tla checks them). A shard's run until it stops is  *)
(* one step. A spare is P: T1 and T2 waiting as spares get no gift (the   *)
(* one gift goes to P or T2) and leave at the close. Thread starts never   *)
(* fail (a failed one leaves the gift with the monitor, which drops it     *)
(* inside the runtime at its end).                                         *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC                                            \* numbers, set sizes, TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    OnRunner,   \* TRUE: the drop runs in a task on W, one of the runtime's runner threads
    Mutation    \* "none", or the name of the one rule this run takes away

\* The dropper's own thread: a user's thread "U", or runner "W".
Me == IF OnRunner THEN "W" ELSE "U"                                          \* who runs the drop

(* --algorithm Teardown                                                      \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    closed = FALSE,          \* `Monitor::close` ran its store
    stopped = FALSE,         \* `stop_all` stopped every shard: a runner's loop ends at its next look
    taken = FALSE,           \* the monitor took S from T1
    lateTake = FALSE,        \* (record) that take came after the close
    detached = FALSE,        \* T1's life says it is out on a taken poll (`Life::detach`)
    refs = 1,                \* strong references to S: T1's to begin with
    lastDrop = "none",       \* where S's last reference went: "none", "runtime" (inside it), "outside"
    gift = "none",           \* who holds the gift of S: "none", "M", "T2", "P" (stored in P's spare), "gone"
    queue = {},              \* the spares the monitor's queue holds (`Monitor::spares`)
    pAlive = TRUE,           \* P's thread still holds its own reference to its spare
    unparked = FALSE,        \* P was unparked and has not looked since
    started = {"T1", "P", "M"} \cup (IF OnRunner THEN {"W"} ELSE {}),  \* the threads the runtime started
    list = {},               \* the threads the drop took to join
    detachedAtSnap = FALSE,  \* T1 was detached when the list was taken
    joining = "none",        \* the thread the drop waits for now
    dropDone = FALSE,        \* `Runtime::drop` returned
    bound = "live",          \* T1's bound task: "live", "done", or "abandoned" (left live at the close)
    deadGift = FALSE;        \* (record) a gift was handed to a spare whose thread had left

define                                                                       \* names the steps and properties use:
    \* Whether thread t has ended: W runs the drop, so it ends only after the drop.
    Finished(t) == IF t = "W" \/ t = "U" THEN FALSE ELSE pc[t] = "Done"     \* a process at its end
    \* The drop never waits on T1 while T1 is blocked in a poll whose shard was taken.
    JoinsNoDetached == ~(joining = "T1" /\ detached /\ pc["T1"] = "T1_poll") \* never such a wait
    \* When the drop returns, every thread the runtime started has ended, or was detached at the list, or is the dropper's own.
    StartedJoined == dropDone =>                                             \* once the drop returned:
        \A t \in started : Finished(t) \/ t = Me \/ (t = "T1" /\ detachedAtSnap)  \* each one is accounted for
    \* The drop never joins its own thread.
    NoSelfJoin == joining # Me                                               \* never itself
    \* A shard's last reference never goes on a thread outside its runtime.
    LastDropInRuntime == lastDrop # "outside"                                \* always inside
    \* The drop returns.
    DropReturns == <>dropDone                                                \* in the end
    \* (Today's design, for the operator) when the drop returns, S is gone with its tasks.
    DropDropsTasks == dropDone => refs = 0                                   \* nobody holds S
    \* (Today's design, for the operator) T1's bound task is never left live at the close.
    BoundNeverLost == bound # "abandoned"                                    \* it ends, never abandoned
    \* Witness: the monitor takes S after the close.
    NoLateTake == ~lateTake                                                  \* claims to: never
    \* Witness: a gift is handed to a spare that had left.
    NoDeadGift == ~deadGift                                                  \* claims to: never
end define;                                                                  \* end of the names

\* One reference to S less, let go of inside the runtime unless `outside`; the last one drops S.
macro LetGo(outside) begin                                                   \* a holder lets go of S
    refs := refs - 1;                                                        \* one fewer
    if refs = 0 then lastDrop := IF outside THEN "outside" ELSE "runtime"; end if;  \* the last: where S drops
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
               /\ (t = "T1" => ~detached \/ Mutation = "no_detached_filter")     \* MUTATION no_detached_filter: T1 even detached
               /\ (t # Me \/ Mutation = "self_join")};                       \* MUTATION self_join: its own thread too
    detachedAtSnap := detached;                                              \* (record) T1 as the list saw it
D_join:                                                                      \* the joins, one at a time
    while list # {} do                                                       \* while one is left
        with t \in list do joining := t; end with;                           \* the next to join
D_wait:                                                                      \* `JoinHandle::join`
        await Finished(joining);                                             \* until it ended
        list := list \ {joining};                                            \* joined
        joining := "none";                                                   \* waiting for none
    end while;                                                               \* every one joined
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
            deadGift := deadGift \/ ~pAlive;                                \* (record) its thread had left
            if pAlive then                                                   \* it lives: its gift is stored, and it unparked
                gift := "P";                                                 \* P takes it at its next look
                unparked := TRUE;                                            \* `spare.thread.unpark()`
            else                                                             \* it left: the popped reference is the last
                gift := "gone";                                              \* `Spare::drop` drops the gift
                \* MUTATION dead_spare_drop: the popped spare is dropped plainly, outside the runtime.
                LetGo(Mutation = "dead_spare_drop");                         \* fixed: through `let_go`, inside it
            end if;                                                          \* handed, or gone
        else                                                                 \* none waits: a runner is started
            started := started \cup {"T2"};                                  \* `spawn_runner`
            gift := "T2";                                                    \* T2 runs S
        end if;                                                              \* handed on
    end if;                                                                  \* the pass is over
    goto M_head;                                                             \* the next tick
end process;                                                                 \* end of the monitor

\* T1, S's first runner, blocked in one poll that returns at some moment.
fair process t1 = "T1"                                                       \* weakly fair: its poll returns
begin                                                                        \* T1's steps:
T1_poll:                                                                     \* in the poll
    skip;                                                                    \* it returns
T1_back:                                                                     \* `come_back`
    if taken then goto T1_home; end if;                                      \* taken: detached
T1_run:                                                                      \* back in: S runs until it stops
    await stopped;                                                           \* the loop's look at `stopped`
    LetGo(FALSE);                                                            \* `drop(shard)`, inside the runtime
    goto T1_spare;                                                           \* then a spare
T1_home:                                                                     \* `finish_home`: until its bound task ends, or the close
    either                                                                   \* one of the two:
        bound := "done";                                                     \* the bound task ended
    or                                                                       \* the close came first:
        await closed;                                                        \* `home_done` is ready
        bound := "abandoned";                                                \* the bound task is left live
    end either;                                                              \* then its reference
T1_drop:                                                                     \* `drop(shard)` in `run_shard`
    LetGo(FALSE);                                                            \* inside the runtime
T1_spare:                                                                    \* a spare until the close
    await closed;                                                            \* it leaves
end process;                                                                 \* end of T1

\* T2, a runner the monitor started for S.
fair process t2 = "T2"                                                       \* weakly fair once started
begin                                                                        \* T2's steps:
T2_start:                                                                    \* the thread starts, or never will
    await "T2" \in started \/ Finished("M");                                 \* started, or the monitor is gone
    if "T2" \notin started then goto Done; end if;                           \* never started
T2_run:                                                                      \* S runs until it stops
    await stopped;                                                           \* the loop's look
    gift := "gone";                                                          \* T2's run is over
    LetGo(FALSE);                                                            \* inside the runtime
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
        LetGo(FALSE);                                                        \* on its thread, inside the runtime
    end if;                                                                  \* gone
    goto Done;                                                               \* ended
P_run:                                                                       \* the gift's shard runs until it stops
    await stopped;                                                           \* the loop's look
    gift := "gone";                                                          \* its run is over
    LetGo(FALSE);                                                            \* inside the runtime
    goto P_push;                                                             \* a spare again
end process;                                                                 \* end of P
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

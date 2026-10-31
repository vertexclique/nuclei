-------------------------------- MODULE Linger --------------------------------
(***************************************************************************)
(* What a dropped handle leaves, when the runtime it ran on drops           *)
(* (src/rt/linger.rs: `hand`, `Inner::linger`, `Guarded`, `Lingering`,      *)
(* `Inner::lingers_done`; src/rt/runtime.rs: `Runtime::drop`,               *)
(* `finish_lingering`, `spawn_linger`; src/rt/runtime/enter.rs:             *)
(* `shutdown_background`; src/rt/sched.rs: `Sched::drop_future`;          *)
(* src/rt/shard.rs: `Shared::settled`, `write_ended`, `drop_stopped`,       *)
(* `release_inbox`;                                                         *)
(* src/fs/call.rs: `WriteCall`, `write_on_pool`; src/fs/ring.rs:            *)
(* `FileOp::complete`, `finish_on_pool`; src/fs/file.rs: `FileState::write` *)
(* polls the write it starts, so its end wakes the task (`K_end`);         *)
(* src/io/handle.rs: the handle's                                          *)
(* drop; src/rt/shard/going.rs: a gone shard's sources).                    *)
(*                                                                          *)
(* Who acts. A worker W of the runtime runs a task that holds a handle     *)
(* whose write-behind holds bytes, and whose file has one write out. The   *)
(* runtime's drop D, on the thread that drops it. The thread K that ends   *)
(* that write (a ring's keeper, the blocking pool). Another thread X that  *)
(* may hold the drain of W's stopped inbox as K's wake lands there. A      *)
(* thread of its own, for a linger nobody else runs.                       *)
(*                                                                          *)
(* What can happen. The task drops the handle while the runtime runs: its  *)
(* linger (the drain of the bytes) is handed to the runtime and runs as a  *)
(* task on W. Or the runtime's drop comes first: from then on lingers go   *)
(* to the runtime's list. The drop stops W, whose shard drops its tasks:   *)
(* the task with the handle (whose drop hands its linger to the list),     *)
(* unless it waits on the write: then only the write's end can drop it,    *)
(* its wake landing in W's stopped inbox, where K or X drops it, inside    *)
(* the task's runtime still. A linger's task not done yet hands what it    *)
(* left back to the list: at once if the stop wakes it, or, held by the    *)
(* write it waits on (`held`), once K ends that write. The write counts    *)
(* itself on W's shard until its end's wake went. Once W is gone, the drop *)
(* runs the list on its own thread until no linger handed to the runtime  *)
(* is left (it counts them), no write is counted on W, and W's inbox is    *)
(* quiet; then it returns. One handed after that runs on a thread of its   *)
(* own. Each linger sends its bytes, one at a time. A background shutdown  *)
(* (`Background`) waits for nothing: it stops W and returns, and every     *)
(* linger runs on a thread of its own.                                     *)
(*                                                                          *)
(* What must never happen. The drop returns before every byte the handle   *)
(* took reached the peer (a background shutdown does not wait, but the    *)
(* bytes still reach it); a linger is dropped unfinished; another runtime  *)
(* (the process default one) is started for a linger.                      *)
(*                                                                          *)
(* What is abstract. One handle and its linger; one write, whose own bytes *)
(* the handle's count leaves out; the bytes are counted; the peer reads (a *)
(* linger that meets a peer that never reads gives up at its own deadline, *)
(* which is one minute, and is not modelled; nor is the drop's own bound   *)
(* of a minute, but as the end of a wait for a linger nothing will hand    *)
(* back); W's stop, with the drop of its tasks, is one step; a task held   *)
(* by something other than the write (a channel another thread keeps) is  *)
(* not modelled: it drops, and hands its linger, when that thread lets it; *)
(* nor is a worker's thread blocked in a task's poll after its shard was   *)
(* handed on, which drops that shard (and what its inbox holds) only once  *)
(* the poll returns: the drop waits for neither.                           *)
(***************************************************************************)
EXTENDS Integers, TLC                                                        \* numbers and TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Total,       \* the bytes in the handle's write-behind
    Background,  \* TRUE: the runtime shuts down in the background, waiting for nothing
    Mutation     \* "none", or the name of the one rule this run takes away

(* --algorithm Linger                                                         \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    closing = FALSE,                                                         \* the runtime's drop began
    wRuns = TRUE,                                                            \* W runs its shard
    dropped = FALSE,                                                         \* the handle was dropped
    linger = "none",                                                         \* where: "none", "task", "held", "list", "alone", "default", "lost", "done"
    sent = 0,                                                                \* bytes the peer got
    over = FALSE,                                                            \* the runtime's drop is over: lingers go alone
    dropDone = FALSE,                                                        \* the runtime's drop returned
    writing = TRUE,                                                          \* the file's write is out: its end has not come
    counted = TRUE,                                                          \* the write still counts itself on W's shard
    taskHeld = FALSE,                                                        \* W stopped while the task waited on the write
    inbox = "empty",                                                         \* W's stopped inbox: "empty", or "pushed" (the held task's wake is there)
    drainer = "none",                                                        \* who drops what the inbox holds: "none", "K", "X"
    defaultStarted = FALSE,                                                  \* (record) the default runtime was started for a linger
    rehanded = FALSE,                                                        \* (record) a linger's task dropped unfinished handed what it left back
    heldOnce = FALSE,                                                        \* (record) a linger's task was held by the write as its worker stopped
    taskHeldOnce = FALSE,                                                    \* (record) the task with the handle was held by the write as W stopped
    lateDrain = FALSE,                                                       \* (record) X dropped the held task, after K counted its write out
    fromList = FALSE;                                                        \* (record) the drop ran a linger of its list

define                                                                       \* names the steps and properties use:
    \* Where a linger handed now goes: a task on W while the runtime runs; the list once its drop began.
    HandedTo == IF over THEN "alone"                                         \* after the drop: a thread of its own
                ELSE IF ~closing THEN "task"                                 \* a worker takes it
                \* MUTATION to_default: during the drop it goes to the process default runtime.
                ELSE IF Mutation = "to_default" THEN "default"               \* which starts for it
                \* MUTATION spawn_on_stopping: it goes to a shard that runs no more.
                ELSE IF Mutation = "spawn_on_stopping" THEN "lost"           \* and is dropped with it
                ELSE "list"                                                  \* else the list the drop runs
    \* Where the held task's linger goes as the task drops on the draining thread.
    \* MUTATION no_context: the task drops outside its runtime: its linger goes to a thread of its own.
    HeldTo == IF Mutation = "no_context" THEN "alone" ELSE HandedTo         \* inside its runtime: as any
    \* A linger that sends one more byte: done once the last went.
    After == IF sent + 1 = Total THEN "done" ELSE linger                     \* the place it runs, or done
    \* A linger the runtime counts: handed to it and not ended (a lost one never ends; the drop's bound ends that wait).
    Counted == linger \in {"task", "held", "list"}                           \* as `Lingering::active` counts
    \* What the drop waits for (`Inner::lingers_done` false), as it looks.
    \* MUTATION no_wait: the drop waits for nothing.
    \* MUTATION list_only: the drop runs what its list holds as it looks, and returns, as the fix's first form did.
    \* MUTATION no_call_wait: the drop does not wait for a write its workers issued.
    \* MUTATION no_inbox_wait: the drop does not wait for a stopped worker's inbox to be drained.
    Waits == /\ Mutation # "no_wait"                                         \* (the mutation: never)
             /\ Mutation # "list_only" \/ linger = "list"                    \* (the mutation: only for the list)
             /\ \/ Counted                                                   \* a linger not ended
                \/ counted /\ Mutation # "no_call_wait"                      \* a write not counted out
                \/ inbox = "pushed" /\ Mutation # "no_inbox_wait"            \* a woken task not dropped
end define;                                                                  \* end of the names

\* The held task drops where its wake was drained: its handle hands its linger.
macro DropHeld() begin                                                       \* `Sched::drop_future`, the handle's drop
    dropped := TRUE;                                                         \* the handle goes
    taskHeld := FALSE;                                                       \* the task is gone
    inbox := "empty";                                                        \* its wake was taken out
    linger := HeldTo;                                                        \* its linger is handed
    if HeldTo = "default" then defaultStarted := TRUE; end if;               \* (the mutation's start)
end macro;                                                                   \* end of the macro

\* The worker: its task drops the handle, the linger's task runs, and the runtime's drop stops it.
fair process worker = "W"                                                    \* weakly fair: it keeps running
begin                                                                        \* the worker's steps:
W_loop:                                                                      \* the next thing to do
    while wRuns do                                                           \* until it is stopped
        either                                                               \* the task drops the handle:
            await ~dropped /\ ~closing;                                      \* while the runtime runs
            dropped := TRUE;                                                 \* the handle goes
            linger := HandedTo;                                              \* its linger is handed: a task here
            if HandedTo = "default" then defaultStarted := TRUE; end if;     \* (never, as the runtime runs)
        or                                                                   \* the linger's task runs:
            await linger = "task";                                           \* it is a task here
            linger := After;                                                 \* one more byte, done at the last
            sent := sent + 1;                                                \* the peer gets it
        or                                                                   \* the runtime's drop stops the worker:
            await closing;                                                   \* the drop began
            wRuns := FALSE;                                                  \* its shard goes, dropping its tasks
            if ~dropped then                                                 \* the task with the handle goes:
                either                                                       \* queued, or waiting on nothing out:
                    dropped := TRUE;                                         \* the handle drops
                    linger := HandedTo;                                      \* its linger is handed during the drop
                    if HandedTo = "default" then defaultStarted := TRUE; end if;  \* (the mutation's start)
                or                                                           \* waiting on the write, still out:
                    await writing;                                           \* only its end can drop the task now
                    taskHeld := TRUE;                                        \* held by it
                    taskHeldOnce := TRUE;                                    \* (recorded)
                end either;                                                  \* one of the two
            elsif linger = "task" then                                       \* a linger's task not done goes:
                \* MUTATION no_rehand: it is dropped with what it had left.
                if Mutation = "no_rehand" then                               \* lost:
                    linger := "lost";                                        \* its bytes never go
                else                                                         \* else `Guarded` hands it back:
                    either                                                   \* the stop woke it:
                        linger := HandedTo;                                  \* to the list now
                        rehanded := TRUE;                                    \* (recorded)
                        if HandedTo = "default" then defaultStarted := TRUE; end if;  \* (the mutation's start)
                    or                                                       \* the write it waits on holds it:
                        await writing;                                       \* still out
                        linger := "held";                                    \* handed back once that write ends
                        heldOnce := TRUE;                                    \* (recorded)
                    end either;                                              \* one of the two
                end if;                                                      \* handed, held or lost
            end if;                                                          \* the shard went
        end either;                                                          \* one of the three
    end while;                                                               \* the worker is gone
end process;                                                                 \* end of the worker

\* The runtime's drop: its lingers go to the list, the worker stops, the list runs, then it returns.
fair process dropper = "D"                                                   \* weakly fair: the drop goes on
begin                                                                        \* the drop's steps:
D_close:                                                                     \* `Lingering::close`
    closing := TRUE;                                                         \* lingers handed from now go to the list
D_join:                                                                      \* `stop_and_join`
    await Background \/ ~wRuns;                                              \* the worker stopped and its thread ended (in the background, not waited for)
D_run:                                                                       \* `finish_lingering`: the list runs here
    while ~Background /\ Waits do                                            \* something may still hand or end a linger (in the background, never looked at)
        await linger = "list" \/ ~Waits;                                     \* one to run, or nothing left: a wake comes
        if linger = "list" then                                              \* one to run:
            linger := After;                                                 \* one more byte, done at the last
            sent := sent + 1;                                                \* the peer gets it
            fromList := TRUE;                                                \* (recorded)
        end if;                                                              \* the run goes on
    end while;                                                               \* nothing left
D_end:                                                                       \* `Lingering::end`
    over := TRUE;                                                            \* lingers handed from now go alone
    if linger = "list" then linger := "alone"; end if;                       \* what is left goes to a thread of its own
    dropDone := TRUE;                                                        \* the drop returns
end process;                                                                 \* end of the drop

\* The thread that ends the write: its slot is filled, its wake goes, then it counts itself out.
fair process ender = "K"                                                     \* weakly fair: the write ends in the end
begin                                                                        \* its steps:
K_count:                                                                     \* before the write's end
    \* MUTATION count_before_wake: the write counts itself out before its end's wake went.
    if Mutation = "count_before_wake" then counted := FALSE; end if;         \* (the mutation's early count)
K_end:                                                                       \* the write ends, at any time
    writing := FALSE;                                                        \* its end is in the slot, and the wake goes:
    if taskHeld then                                                         \* the held task's wake lands in W's stopped inbox:
        inbox := "pushed";                                                   \* left there
        either drainer := "K";                                               \* this thread takes the drain
        or drainer := "X";                                                   \* another thread holds it, and drains what came
        end either;                                                          \* one of the two
    elsif linger = "held" then                                               \* the held linger's task drops: `Guarded`
        linger := HandedTo;                                                  \* hands it back where lingers go now
        rehanded := TRUE;                                                    \* (recorded)
        if HandedTo = "default" then defaultStarted := TRUE; end if;         \* (the mutation's start)
    end if;                                                                  \* the wake went
K_drain:                                                                     \* `drop_stopped`, on this thread
    if drainer = "K" then DropHeld(); drainer := "none"; end if;             \* the task drops here
K_out:                                                                       \* `WriteCall`'s drop
    counted := FALSE;                                                        \* the write counts itself out
end process;                                                                 \* end of the thread

\* Another thread that held the drain of W's stopped inbox as the wake landed: it drops what came, later.
fair process other = "X"                                                     \* weakly fair: its drain goes on
begin                                                                        \* its one step:
X_drain:                                                                     \* its drain, after K went on
    await drainer = "X" \/ (pc["K"] = "Done" /\ drainer = "none");          \* its part, or none came
    if drainer = "X" then                                                    \* the held task's wake is its to drop:
        DropHeld();                                                          \* the task drops here
        drainer := "none";                                                   \* drained
        lateDrain := TRUE;                                                   \* (recorded)
    end if;                                                                  \* done
end process;                                                                 \* end of the thread

\* A thread of its own, or the default runtime: what nobody else runs.
fair process elsewhere = "E"                                                 \* weakly fair: it runs in the end
begin                                                                        \* its steps:
E_run:                                                                       \* until its linger is done
    while linger \in {"alone", "default"} do                                 \* a linger is here
        linger := After;                                                     \* one more byte, done at the last
        sent := sent + 1;                                                    \* the peer gets it
    end while;                                                               \* none is (any more)
    await (dropDone /\ pc["W"] = "Done" /\ pc["K"] = "Done" /\ pc["X"] = "Done")  \* nothing more comes once all are done
          \/ linger \in {"alone", "default"};                                \* or one came meanwhile
    if linger \in {"alone", "default"} then goto E_run; end if;              \* one that came meanwhile
end process;                                                                 \* end of the thread
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* The values every variable may take.
TypeOK == /\ sent \in 0..Total                                               \* bytes sent
          /\ linger \in {"none", "task", "held", "list", "alone", "default", "lost", "done"}  \* where the linger is
          /\ inbox \in {"empty", "pushed"}                                       \* W's stopped inbox
          /\ drainer \in {"none", "K", "X"}                                      \* who drops what it holds

\* When the runtime's drop returns, every byte the handle took reached the peer.
DropWaits == dropDone => sent = Total                                        \* nothing is left behind

\* No runtime is started for a linger behind the user's back.
NoDefault == ~defaultStarted                                                 \* never

\* Every byte reaches the peer in the end.
AllSent == <>(sent = Total)                                                  \* the peer gets them all

\* `Termination` (every process ends) is the one the translator writes for the fair processes.

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A linger's task dropped unfinished as its worker stopped hands what it left back.
NoRehand == ~rehanded                                                        \* never handed back
\* The drop runs a linger from its list.
NoFromList == ~fromList                                                      \* never from the list
\* A linger's task is held by the write as its worker stops, and hands it back later.
NoHeld == ~heldOnce                                                          \* never held
\* The task with the handle is held by the write as W stops, and dropped where the write ends.
NoTaskHeld == ~taskHeldOnce                                                  \* never held
\* Another thread drops the held task after the write counted itself out.
NoLateDrain == ~lateDrain                                                    \* never late
=============================================================================

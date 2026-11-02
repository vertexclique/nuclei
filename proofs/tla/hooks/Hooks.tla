-------------------------------- MODULE Hooks --------------------------------
(***************************************************************************)
(* A runner thread's hooks that may panic (src/rt/tuning.rs:               *)
(* `ThreadHooks::started`, `ThreadHooks::parked`, `ThreadHooks::unparked`, *)
(* `Stopping`, `call_caught`, `name_caught`; src/rt/workers.rs: `runner`,  *)
(* `spawn_runner`, `spawn_thread`; src/rt/shard/run.rs:                    *)
(* `Shard::run_loop`'s park; src/rt/handoff/monitor.rs: `give`, whose gift *)
(* is a shard's only strong reference).                                    *)
(*                                                                          *)
(* Who acts. A runner R that holds shard S, its one strong reference (the  *)
(* monitor's gift to a runner it started, or a worker's run as its thread  *)
(* starts), with a task of S queued. A waker K that wakes another task of  *)
(* S at some moment. Before R exists, the monitor names R's thread with    *)
(* the user's name function, the gift already handed to the thread to be   *)
(* (`Handed`). R calls the user's start hook, runs what S queues, and      *)
(* parks with nothing to run: it says it parks, looks once more, calls the *)
(* park hook, sleeps until notified, ends its park and calls the unpark    *)
(* hook. K pushes its task to S and notifies a parked S.                   *)
(*                                                                          *)
(* What can happen. Any call of a hook, or of the name function, may       *)
(* panic. A panic the runtime catches where it calls the hook (after the   *)
(* panic hook reported it) is let go of, and R goes on; the name's, caught *)
(* the same way, leaves R the default name. One that unwinds leaves        *)
(* `runner` (or, the name's, the monitor's `give`): the reference to S     *)
(* goes, the last one, and S is dropped with the tasks it queues, while    *)
(* the runtime runs.                                                       *)
(*                                                                          *)
(* What must never happen. S is dropped while the runtime runs, its tasks  *)
(* with it, each awaited for an output that never comes. And every task    *)
(* runs in the end.                                                         *)
(*                                                                          *)
(* What is abstract. S's queue and inbox are one count. The park's         *)
(* handshake is one flag and one notification (tla/park/Park.tla checks it *)
(* at the grain of its accesses). The stop hook is called last, as the     *)
(* thread ends, where nothing is left to lose; a panic in it is caught as  *)
(* well (one while the thread unwinds would abort the process), and is not *)
(* modelled. A worker's own first thread is named in `start_one` before    *)
(* anything is claimed or made: a panic there reaches the caller of        *)
(* `build` or `spawn_more_threads` with nothing held, and is not modelled. *)
(***************************************************************************)
EXTENDS Integers, TLC                                                        \* numbers and TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Mutation    \* "none", or the name of the one call (a hook's, or the name's) whose panic unwinds instead of being caught

Total == 2                                                                   \* the tasks: the queued one and K's

(* --algorithm Hooks                                                         \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    queued = 1,        \* tasks of S queued and not run: the gift's one to begin with
    ran = 0,           \* tasks run
    woken = FALSE,     \* K woke its task
    parked = FALSE,    \* S says it parks
    pending = FALSE,   \* a notification R has not taken
    freed = FALSE,     \* S was dropped: its last reference went
    dropped = 0,       \* tasks dropped unrun with S
    caught = FALSE;    \* (record) a hook's or the name's panic was caught and R went on

define                                                                       \* names the steps and properties use:
    \* S is never dropped while the runtime runs.
    NoLoss == ~freed                                                         \* never
    \* Every task runs in the end.
    AllRan == <>(ran = Total)                                                \* both of them
    \* No task is run twice or made up.
    RunOnce == ran + queued + dropped <= Total                               \* never more than were made
    \* Witness: a hook or the name function panics, the panic is caught, and R goes on.
    NoCaught == ~caught                                                      \* claims to: never
end define;                                                                  \* end of the names

\* A hook's (or the name's) call panics: caught where it is called, and R goes on; or, under `site`'s mutation, it unwinds.
macro Panic(site) begin                                                      \* a panic in the call `site`
    if Mutation = site then                                                  \* (mutated) it unwinds:
        goto R_unwind;                                                       \* out of `runner`, or the monitor's `give`
    else                                                                     \* `call_caught`, `name_caught`:
        caught := TRUE;                                                      \* let go of
    end if;                                                                  \* R goes on
end macro;                                                                   \* end of the macro

\* The runner (`runner`, then `Shard::run_loop`).
fair process runner = "R"                                                    \* weakly fair: it runs
begin                                                                        \* R's steps:
R_name:                                                                      \* `spawn_runner` names R's thread, on the monitor's, the gift in `Handed`
    either skip;                                                             \* the name function returns
    or Panic("name_unwinds");                                                \* MUTATION name_unwinds: it panics, uncaught
    end either;                                                              \* one of the two: R starts
R_start:                                                                     \* `ThreadHooks::started`
    either skip;                                                             \* the start hook returns
    or Panic("start_unwinds");                                               \* MUTATION start_unwinds: it panics, uncaught
    end either;                                                              \* one of the two
R_run:                                                                       \* the loop runs what S queues
    if queued > 0 then                                                       \* a task to run:
        queued := queued - 1;                                                \* taken
        ran := ran + 1;                                                      \* and run
        goto R_run;                                                          \* the next one
    end if;                                                                  \* nothing left
R_publish:                                                                   \* `begin_park`: it says it parks, and looks once more
    if queued > 0 then goto R_run;                                           \* work came: it backs out
    elsif ran = Total then goto Done;                                        \* (the model's end: nothing more comes)
    else parked := TRUE;                                                     \* it parks
    end if;                                                                  \* the look is over
R_park_hook:                                                                 \* `ThreadHooks::parked`
    either skip;                                                             \* the park hook returns
    or Panic("park_unwinds");                                                \* MUTATION park_unwinds: it panics, uncaught
    end either;                                                              \* one of the two
R_sleep:                                                                     \* the driver's wait, until notified
    await pending;                                                           \* K's notification
    pending := FALSE;                                                        \* taken
R_end:                                                                       \* `end_park`
    parked := FALSE;                                                         \* awake
R_unpark_hook:                                                               \* `ThreadHooks::unparked`
    either skip;                                                             \* the unpark hook returns
    or Panic("unpark_unwinds");                                              \* MUTATION unpark_unwinds: it panics, uncaught
    end either;                                                              \* one of the two
R_again:                                                                     \* back to the loop
    goto R_run;                                                              \* the next look at the queue
R_unwind:                                                                    \* the panic leaves `runner`, or the monitor's `give`
    freed := TRUE;                                                           \* the reference to S (R's, or `Handed`'s), the last, goes
    dropped := queued;                                                       \* what S queues is dropped unrun
    queued := 0;                                                             \* gone with it
end process;                                                                 \* end of the runner

\* The waker: another task of S is woken from another thread.
fair process waker = "K"                                                     \* weakly fair: it wakes
begin                                                                        \* K's steps:
K_wake:                                                                      \* `Shared::push_remote`, then the handshake
    if freed then                                                            \* S went: the task is dropped where it was pushed
        dropped := dropped + 1;                                              \* lost with S
    else                                                                     \* S is there:
        queued := queued + 1;                                                \* pushed
        if parked then parked := FALSE; pending := TRUE; end if;             \* a parked S is notified
    end if;                                                                  \* either way
    woken := TRUE;                                                           \* (record)
end process;                                                                 \* end of the waker
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

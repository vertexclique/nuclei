------------------------------ MODULE Asleep -------------------------------
(***************************************************************************)
(* How the monitor sleeps while every worker is parked, and how a worker   *)
(* wakes it (`run` in src/rt/handoff/monitor.rs: `asleep`, `all_parked`,   *)
(* `worker_woke`; the worker's park in src/rt/shard/run.rs, `run_loop` and *)
(* `turn`, with `Handshake::begin_park` in src/rt/handshake.rs).            *)
(*                                                                          *)
(* Who acts. A worker runs its shard; with nothing to run it says it parks *)
(* (`parked`), looks for work once more, and parks, or backs out of the     *)
(* park when work came. Work comes from another thread, which clears the   *)
(* flag of a parked worker and wakes it. The monitor watches the workers   *)
(* while any runs; with every one parked it says it sleeps (`asleep`),     *)
(* looks at the parks again, and sleeps until a worker rings it.           *)
(*                                                                          *)
(* What can happen. The monitor's two looks may both fall in the moment a  *)
(* worker says it parks and then backs out: the monitor sleeps while the   *)
(* worker runs on. The worker's next poll may block for good, and only the *)
(* monitor, awake, hands the shard on (proofs/tla/handoff/Handoff.tla).    *)
(*                                                                          *)
(* What must never happen. The monitor sleeps for good while a worker      *)
(* runs: a blocked poll is then never rescued. So a worker that backs out  *)
(* of its park, as one whose park ends, rings a monitor that sleeps. The   *)
(* accesses are sequentially consistent, as in the code; the store-load    *)
(* pairs on both sides are what makes one side see the other.              *)
(*                                                                          *)
(* What is abstract. One worker, one piece of work that comes once, a poll *)
(* that blocks for good once it runs; the bell keeps one ring until it is  *)
(* waited for, as a thread's park token does.                              *)
(***************************************************************************)
EXTENDS Integers, TLC                                                        \* numbers and TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Mutation    \* "none", or the name of the one rule this run takes away

(* --algorithm Asleep                                                        \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    parked = FALSE,      \* the worker says it parks
    work = FALSE,        \* work came for the worker
    asleep = FALSE,      \* the monitor says it sleeps
    rung = FALSE,        \* a ring the monitor's bell keeps
    running = FALSE,     \* the worker runs the work: a poll that blocks for good
    backedOut = FALSE;   \* (record) the worker backed out of a park while the monitor slept

define                                                                       \* names the properties use:
    \* While the worker runs its blocked poll, the monitor is awake in the end.
    Watched == running ~> ~asleep                                            \* a running worker is watched
    \* Witness: a worker backs out of its park while the monitor sleeps.
    NoBackOutAsleep == ~backedOut                                            \* claims to: never
end define;                                                                  \* end of the names

\* A worker's ring of a monitor that sleeps (`worker_woke`): a look at `asleep` after the
\* worker's sequentially consistent store of its flag.
macro ring() begin                                                           \* the worker rings
    if asleep then rung := TRUE; end if;                                     \* only a monitor that sleeps
end macro;                                                                   \* end of the ring

\* The other thread: it pushes the work, then wakes a parked worker (`Handshake::wake`).
fair process pusher = "pusher"                                               \* weakly fair: it acts
begin                                                                        \* the pusher's steps:
P_push:                                                                      \* the work is pushed
    work := TRUE;                                                            \* (a sequentially consistent push)
P_wake:                                                                      \* the worker woken if it parks
    parked := FALSE;                                                         \* its flag cleared (the swap)
end process;                                                                 \* end of the pusher

\* The worker: it parks while it has nothing to run, and runs the work once it came.
fair process worker = "worker"                                               \* weakly fair: it acts
begin                                                                        \* the worker's steps:
W_say:                                                                       \* `begin_park`: it says it parks
    parked := TRUE;                                                          \* (a sequentially consistent store)
W_look:                                                                      \* the look for work after the fence
    if work then                                                             \* work came: it backs out
        parked := FALSE;                                                     \* (a sequentially consistent store)
        backedOut := backedOut \/ asleep;                                    \* (record) the monitor slept meanwhile
        \* MUTATION no_backout_ring: the back-out rings nobody, as before the fix.
        if Mutation # "no_backout_ring" then ring(); end if;                 \* a monitor that sleeps is rung
        goto W_run;                                                          \* and the work runs
    end if;                                                                  \* else it parks
W_park:                                                                      \* parked until woken
    await ~parked;                                                           \* the pusher cleared its flag
W_woke:                                                                      \* `end_park`, `parked_ended`
    \* MUTATION no_wake_ring: the park's end rings nobody.
    if Mutation # "no_wake_ring" then ring(); end if;                        \* a monitor that sleeps is rung
    if ~work then goto W_say; end if;                                        \* woken with nothing: it parks again
W_run:                                                                       \* the work runs, and its poll blocks
    running := TRUE;                                                         \* for good: only a handoff helps
end process;                                                                 \* end of the worker

\* The monitor: it watches while the worker runs, and sleeps while it parks.
fair process monitor = "monitor"                                             \* weakly fair: it acts
begin                                                                        \* the monitor's steps:
M_look:                                                                      \* its first look at the parks
    while TRUE do                                                            \* for as long as the runtime runs
        if parked then                                                       \* every worker parked:
M_say:                                                                       \* it says it sleeps
            asleep := TRUE;                                                  \* (a sequentially consistent store)
M_again:                                                                     \* and looks again after the fence
            \* MUTATION no_second_look: the monitor sleeps on its first look.
            if parked \/ Mutation = "no_second_look" then                    \* still parked:
M_sleep:                                                                     \* it sleeps
                await rung;                                                  \* until a worker rings
                rung := FALSE;                                               \* the ring is spent
            end if;                                                          \* awake
M_up:                                                                        \* it says it is awake
            asleep := FALSE;                                                 \* and watches again
        end if;                                                              \* a tick of its watch
    end while;                                                               \* the next look
end process;                                                                 \* end of the monitor
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

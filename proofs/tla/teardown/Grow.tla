-------------------------------- MODULE Grow ---------------------------------
(***************************************************************************)
(* A worker started while its runtime is dropped (src/rt/workers.rs:      *)
(* `Workers::start_one`, `spawn_thread`, `start_monitor`, `take_monitor`, *)
(* `take_threads`, `stop_all`; src/rt/runtime.rs: `stop_and_join`;        *)
(* src/rt/runtime/enter.rs: `shutdown_background`; src/compat.rs:          *)
(* `spawn_more_threads`).                                                  *)
(*                                                                          *)
(* Who acts. The dropper D. A starter S, a task of the runtime that asks   *)
(* for one more worker as the drop runs. N, that worker's thread once      *)
(* started. M, the monitor's thread, when S's start is the runtime's first *)
(* (it was built with no worker) and starts the monitor too.               *)
(*                                                                          *)
(* What can happen. The drop closes the monitor, stops every shard in its  *)
(* slot, takes the monitor's thread and joins it, then takes the list of   *)
(* threads with a monitor started since, closing the list (no thread      *)
(* starts from then on), stops every shard in its slot once more, and      *)
(* joins what it took. S, under the list's lock, looks at the close (and   *)
(* starts nothing if the list is closed), starts N's thread, puts it on    *)
(* the list and publishes N's shard in its slot, then lets go of the lock; *)
(* a first start then starts the monitor, under the lock again, unless the *)
(* list is closed. Any step of S may come between any two of the drop's.   *)
(*                                                                          *)
(* What must never happen. The drop waits for good on N, whose shard no    *)
(* stop reached; the drop returns while N or M still runs, never joined.   *)
(*                                                                          *)
(* What is abstract. A worker's run until its shard stops, then its leave  *)
(* as a spare that sees the close, is one step; so is a monitor's run      *)
(* until it sees the close (tla/teardown/Teardown.tla checks a drop        *)
(* against a monitor's pass). N's thread always starts (a failed start is  *)
(* tla/placement/Publish.tla's). The drop's takes are one step each under  *)
(* the lock, which only S holds across steps.                              *)
(***************************************************************************)
EXTENDS Integers, TLC                                                        \* numbers and TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Mutation    \* "none", or the name of the one rule this run takes away

(* --algorithm Grow                                                          \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    closed = FALSE,       \* the drop closed the monitor (`Monitor::close`, in `stop_all`)
    lock = "none",        \* who holds the list's lock across steps: "none" or "S"
    listClosed = FALSE,   \* the drop took the list and closed it: no thread starts from then on
    nStarted = FALSE,     \* N's thread was started
    nListed = FALSE,      \* N's handle is on the list
    published = FALSE,    \* N's shard is in its slot, where a stop finds it
    nStopped = FALSE,     \* a stop reached N's shard
    mStarted = FALSE,     \* the monitor's thread was started by S
    mHandle = FALSE,      \* the monitor's handle is in its place, for the drop to take
    monJoin = FALSE,      \* the drop took the monitor's handle, to join it first
    list = {},            \* the threads the drop took to join after
    joining = "none",     \* the thread the drop waits for now
    dropDone = FALSE,     \* the drop returned
    between = FALSE,      \* (record) S published N's shard after the drop's first stop and before its take
    refused = FALSE;      \* (record) S found the list closed, and started nothing

define                                                                       \* names the steps and properties use:
    \* Whether thread t has ended.
    Finished(t) == pc[t] = "Done"                                            \* a process at its end
    \* When the drop returns, every thread S started has ended.
    StartedJoined == dropDone =>                                             \* once the drop returned:
        /\ (nStarted => Finished("N"))                                       \* N, if started, ended
        /\ (mStarted => Finished("M"))                                       \* M, if started, ended
    \* The drop returns.
    DropReturns == <>dropDone                                                \* in the end
    \* Witness: S publishes N's shard between the drop's first stop and its take.
    NoBetween == ~between                                                    \* claims to: never
    \* Witness: S finds the list closed and starts nothing.
    NoRefused == ~refused                                                    \* claims to: never
end define;                                                                  \* end of the names

\* The drop (`Runtime::drop`, `stop_and_join`; `shutdown_background` takes the same steps but the joins).
fair process dropper = "D"                                                   \* weakly fair: the drop goes on
begin                                                                        \* the drop's steps:
D_close:                                                                     \* `lingering.close`, then `Monitor::close`
    closed := TRUE;                                                          \* the monitor and its spares stop
D_stop:                                                                      \* `stop_all`: every shard in its slot
    if published then nStopped := TRUE; end if;                              \* N's, if it is there
D_monitor:                                                                   \* `take_monitor`, under the lock
    await lock = "none";                                                     \* S does not hold it
    monJoin := mHandle;                                                      \* the monitor's handle, if one is there
    mHandle := FALSE;                                                        \* taken
D_monjoin:                                                                   \* the monitor joined first
    if monJoin then await Finished("M"); end if;                             \* until it ended
D_take:                                                                      \* `take_threads`, under the lock
    await lock = "none";                                                     \* S does not hold it
    \* MUTATION monitor_untaken: a monitor started since `take_monitor` is not taken with the list.
    list := (IF nListed THEN {"N"} ELSE {})                                  \* N, if it is on the list
            \cup (IF mHandle /\ Mutation # "monitor_untaken" THEN {"M"} ELSE {});  \* a monitor started since
    nListed := FALSE;                                                        \* the list is emptied
    mHandle := mHandle /\ Mutation = "monitor_untaken";                      \* taken, but under the mutation
    listClosed := TRUE;                                                      \* no thread starts from now on
D_restop:                                                                    \* `stop_all` once more
    \* MUTATION no_restop: no stop after the take, as before the fix.
    if Mutation # "no_restop" /\ published then nStopped := TRUE; end if;    \* N's shard, published before the take
D_join:                                                                      \* the joins, one at a time
    while list # {} do                                                       \* while one is left
        with t \in list do joining := t; end with;                           \* the next to join
D_wait:                                                                      \* `JoinHandle::join`
        await Finished(joining);                                             \* until it ended
        list := list \ {joining};                                            \* joined
        joining := "none";                                                   \* waiting for none
    end while;                                                               \* every one joined
D_done:                                                                      \* the drop returns
    dropDone := TRUE;                                                        \* done
end process;                                                                 \* end of the drop

\* The starter (`spawn_more_threads`, `Workers::start_one`, `spawn_thread`, `start_monitor`).
fair process starter = "S"                                                   \* weakly fair: it goes on
begin                                                                        \* S's steps:
S_lock:                                                                      \* `spawn_thread` takes the list's lock
    await lock = "none";                                                     \* free
    lock := "S";                                                             \* held
S_look:                                                                      \* its look at the close
    \* MUTATION start_ignores_close: the worker's thread starts after the list was taken, as before the fix.
    if listClosed /\ Mutation # "start_ignores_close" then                   \* the drop took the list:
        lock := "none";                                                      \* let go
        refused := TRUE;                                                     \* (record) none started
        goto Done;                                                           \* `start_one` returns none
    end if;                                                                  \* else the start goes on
S_spawn:                                                                     \* the thread starts, and is put on the list
    nStarted := TRUE;                                                        \* N's thread runs
    nListed := TRUE;                                                         \* its handle on the list
    \* MUTATION publish_unlocked: the lock is let go before the publish, as before the fix.
    if Mutation = "publish_unlocked" then lock := "none"; end if;            \* (mutated) let go early
S_publish:                                                                   \* N's shard into its slot
    published := TRUE;                                                       \* where a stop finds it
    between := pc["D"] \in {"D_monitor", "D_monjoin", "D_take"};             \* (record) between the stop and the take
    if Mutation # "publish_unlocked" then lock := "none"; end if;            \* the lock let go
S_first:                                                                     \* `start_monitor`: was the monitor started?
    either goto Done;                                                        \* it runs already: S is done
    or skip;                                                                 \* this start is the first: it starts it
    end either;                                                              \* one of the two
S_mlock:                                                                     \* the lock again
    await lock = "none";                                                     \* free
    lock := "S";                                                             \* held
S_monitor:                                                                   \* its look at the close, then the start
    \* MUTATION monitor_ignores_close: the monitor starts after the list was taken.
    if ~listClosed \/ Mutation = "monitor_ignores_close" then                \* the list is open:
        mStarted := TRUE;                                                    \* the monitor's thread runs
        mHandle := TRUE;                                                     \* its handle in its place
    end if;                                                                  \* else none is started
    lock := "none";                                                          \* let go
end process;                                                                 \* end of S

\* N, the new worker's thread (`runner`).
fair process worker = "N"                                                    \* weakly fair once started
begin                                                                        \* N's steps:
N_start:                                                                     \* started, or never will be
    await nStarted \/ Finished("S");                                         \* S started it, or S is done
    if ~nStarted then goto Done; end if;                                     \* never started
N_run:                                                                       \* its shard runs until it stops, then it leaves
    await nStopped;                                                          \* a stop reached it
end process;                                                                 \* end of N

\* M, the monitor's thread S may start (`monitor::run`).
fair process monitor = "M"                                                   \* weakly fair once started
begin                                                                        \* M's steps:
M_start:                                                                     \* started, or never will be
    await mStarted \/ Finished("S");                                         \* S started it, or S is done
    if ~mStarted then goto Done; end if;                                     \* never started
M_run:                                                                       \* it runs until its look at the close
    await closed;                                                            \* closed: it ends
end process;                                                                 \* end of M
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

-------------------------------- MODULE Start --------------------------------
(***************************************************************************)
(* A blocking pool's thread start that fails, and a start hook or a name   *)
(* function that panics (src/rt/blocking.rs: `Pool::submit`,               *)
(* `start_thread`, `count_thread`, `cannot_start`, `work`;                 *)
(* src/rt/tuning.rs: `ThreadHooks::started`, `call_caught`,                *)
(* `name_caught`).                                                         *)
(*                                                                         *)
(* Who acts. Two submitters, each sending one job, and the pool's threads, *)
(* at most `Max`. A submitter pushes its job, then starts a thread while   *)
(* the pool has room: it counts the thread first (a compare-and-swap below *)
(* the cap), names it with the user's name function, then asks the system  *)
(* for it. The start may fail: the submitter takes its count back, and, if *)
(* no thread is counted then, runs the queued jobs itself, as a last       *)
(* resort, for with no thread they would wait for good. A thread that      *)
(* starts calls the user's start hook, then takes jobs until the queue is  *)
(* empty, leaves the count and looks at the queue once more, taking a job  *)
(* back (counted again, below the cap) that came while it still counted.   *)
(*                                                                         *)
(* What can happen. Any start may fail, and any start hook or name may     *)
(* panic. The hook's panic is caught where it is called (after the panic   *)
(* hook reported it), and the thread goes on; the name's is caught the     *)
(* same way, and the thread takes the default name.                        *)
(*                                                                         *)
(* What must never happen. A job never runs: no thread will take it, and   *)
(* nobody runs it. A job runs twice. The pool counts past its cap.         *)
(*                                                                         *)
(* What is abstract. The idle wait and its wakes (tla/pool/Pool.tla and    *)
(* PoolIndex.tla check them): a thread with nothing to take leaves at      *)
(* once. The slots are left out.                                           *)
(***************************************************************************)
EXTENDS Integers, TLC                                                        \* numbers and TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Max,        \* the pool's most threads
    Mutation    \* "none", or the name of the one rule this run takes away

Submitters == {11, 12}                                                       \* the submitting threads
Threads == 1..Max                                                            \* the pool's threads
Total == 2                                                                   \* every job sent: one each

(* --algorithm Start                                                         \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    queue = 0,          \* jobs pushed and not taken
    ran = 0,            \* jobs run
    threads = 0,        \* the pool's count of its threads
    tokens = 0,         \* threads the system started that have not run yet
    inline = FALSE,     \* (record) a submitter ran jobs itself after a failed start
    caught = FALSE;     \* (record) a start hook's or a name's panic was caught

define                                                                       \* names the properties use:
    \* No job runs twice: runs and queued never pass the jobs sent.
    RunOnce == ran + queue <= Total /\ queue >= 0                            \* never more
    \* The pool keeps to its cap.
    Bounded == threads <= Max /\ threads >= 0                                \* the count stays in range
    \* Every job runs in the end.
    AllRan == <>(ran = Total)                                                \* both of them
    \* Witness: a submitter runs jobs itself after a failed start.
    NoInline == ~inline                                                      \* claims to: never
    \* Witness: a start hook or a name panics and the thread goes on.
    NoCaught == ~caught                                                      \* claims to: never
end define;                                                                  \* end of the names

\* A submitter (`Pool::submit`, `start_thread`, `cannot_start`).
fair process submitter \in Submitters                                        \* weakly fair: it goes on
variables zero = FALSE;                                                      \* what its look at the count read
begin                                                                        \* a submitter's steps:
S_push:                                                                      \* the job into the queue
    queue := queue + 1;                                                      \* pushed
S_count:                                                                     \* `count_thread`: one more, below the cap
    if threads < Max then threads := threads + 1;                            \* counted
    else goto Done;                                                          \* no room: the threads there take it
    end if;                                                                  \* counted
S_name:                                                                      \* the thread's name, from the user's function
    either skip;                                                             \* it returns
    or                                                                       \* it panics:
        \* MUTATION name_unwinds: the panic leaves `start_thread`, as before the fix: the count held, no thread asked for.
        if Mutation = "name_unwinds" then goto Done;                         \* (mutated) the submit is over
        else caught := TRUE;                                                 \* `name_caught`: the default name
        end if;                                                              \* one of the two
    end either;                                                              \* now the start
S_spawn:                                                                     \* the system's start
    either tokens := tokens + 1; goto Done;                                  \* the thread starts
    or skip;                                                                 \* the start fails
    end either;                                                              \* one of the two
S_first:                                                                     \* the count back, then the look (today's order)
    \* MUTATION check_before_decrement: the look at the count comes before its own count goes.
    if Mutation = "check_before_decrement" then zero := threads = 0;         \* (mutated) its own count still in
    else threads := threads - 1;                                             \* its count goes
    end if;                                                                  \* first of the two
S_second:                                                                    \* the other of the two
    if Mutation = "check_before_decrement" then threads := threads - 1;      \* (mutated) its count goes last
    else zero := threads = 0;                                                \* no thread counted?
    end if;                                                                  \* both done
S_inline:                                                                    \* with none, the queue runs here
    if zero then                                                             \* no thread will take them:
        inline := TRUE;                                                      \* (record)
        ran := ran + queue;                                                  \* each queued job runs here
        queue := 0;                                                          \* none left
    end if;                                                                  \* the submit is over
end process;                                                                 \* end of a submitter

\* A pool thread (`Pool::work`).
fair process thread \in Threads                                              \* weakly fair: it runs
begin                                                                        \* a thread's steps:
T_dormant:                                                                   \* not running
    await tokens > 0 \/ \A s \in Submitters : pc[s] = "Done";               \* a start for it, or none can come
    if tokens = 0 then goto Done;                                            \* (the model's end)
    else tokens := tokens - 1;                                               \* taken
    end if;                                                                  \* one of the two
T_hook:                                                                      \* `ThreadHooks::started`
    either skip;                                                             \* the start hook returns
    or                                                                       \* it panics:
        \* MUTATION hook_unwinds: the panic leaves `work`, as before the fix: the thread is gone, still counted.
        if Mutation = "hook_unwinds" then goto Done;                         \* (mutated) gone
        else caught := TRUE;                                                 \* `call_caught`: it goes on
        end if;                                                              \* one of the two
    end either;                                                              \* now the jobs
T_take:                                                                      \* the jobs, one at a time
    if queue > 0 then                                                        \* one is there:
        queue := queue - 1;                                                  \* taken
        ran := ran + 1;                                                      \* and run
        goto T_take;                                                         \* the next
    end if;                                                                  \* the queue is empty
T_leave:                                                                     \* it leaves the count
    threads := threads - 1;                                                  \* one fewer
T_last:                                                                      \* its last look, counted again below the cap
    if queue > 0 /\ threads < Max then                                       \* a job came meanwhile, and room:
        threads := threads + 1;                                              \* counted again
        goto T_take;                                                         \* it takes it
    else                                                                     \* else it is gone:
        goto T_dormant;                                                      \* a later start may run here again
    end if;                                                                  \* one of the two
end process;                                                                 \* end of a thread
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

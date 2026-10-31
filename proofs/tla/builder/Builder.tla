------------------------------ MODULE Builder ------------------------------
(***************************************************************************)
(* A runtime's settings, checked once when it is built, and the process   *)
(* default runtime.                                                        *)
(*                                                                          *)
(* The Rust it models: `Builder::validate`, `Builder::worker_count`,       *)
(* `Builder::build` and `Builder::build_global`                            *)
(* (src/rt/runtime/builder.rs); the setters, among them `event_interval`,  *)
(* `global_queue_interval`, `max_io_events_per_tick` and `on_long_poll`   *)
(* (src/rt/runtime/knobs.rs), which only record their value; `validate`   *)
(* of a list of cores (src/rt/runtime/cores.rs);                           *)
(* `IoUringConfiguration::validate` (src/config.rs); `init_with_config`    *)
(* and `GlobalExecutorConfig::builder` (src/compat.rs); and `try_default`  *)
(* (src/rt/runtime/fallible.rs), which starts the default runtime for the  *)
(* free functions.                                                         *)
(*                                                                          *)
(* In plain words: a program makes a builder and calls its setters in any  *)
(* order, each any number of times. A setter only records its value, the  *)
(* last call winning, and never panics. Then the program calls build: the  *)
(* settings are checked in one place (validate), each against its range    *)
(* and against the others, in a fixed order, and the first one that does   *)
(* not fit is refused with InvalidInput naming it. Otherwise the runtime   *)
(* starts with the workers worker_count gives, unless the machine refuses  *)
(* (a backend this process cannot have, a poller, a thread), which is      *)
(* never InvalidInput. Two calls may race to make the process default      *)
(* runtime. build_global looks first and answers AlreadyExists while one   *)
(* runs; two that both looked before either set it both build, the second  *)
(* set fails, and that call answers AlreadyExists too, its runtime dropped *)
(* (its workers stop). init_with_config builds the default from the old    *)
(* executor configuration and logs any failure but AlreadyExists at the    *)
(* error level. A free function starts the default with every setting at  *)
(* its default when none runs, and keeps one that another call set first.  *)
(*                                                                          *)
(* What must never happen: a setter that panics; a build that answers Ok   *)
(* with a setting out of range or two that do not fit, or InvalidInput     *)
(* with every setting fine; an InvalidInput naming a setting that is fine; *)
(* a runtime whose count of workers is not the documented one, or that may *)
(* run more than max_worker_threads; a build_global that answers anything  *)
(* but AlreadyExists while the default runs, or leaves a runtime of its own *)
(* running when it answers AlreadyExists; a second default; an init whose  *)
(* failure goes unlogged, or that logs AlreadyExists.                      *)
(*                                                                          *)
(* What is abstract: each setting takes a few values, each standing for    *)
(* every value on the same side of the setting's bounds; the process may   *)
(* run on every core of the machine; the machine's refusal is one choice   *)
(* at the build, where a run lets the machine refuse. Left out: settings   *)
(* whose every value is accepted (names, keep-alive, pinning, the margin,  *)
(* the seed, the hooks other than on_long_poll, the backend, which only    *)
(* the machine may refuse); io_buffer_cap, which a worker checks against   *)
(* its ring's buffers when it starts; the environment variables            *)
(* (NUCLEI_BACKEND, the executor configuration's own); and the lock that   *)
(* lets one free function start the default at a time (one caller here).  *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets                          \* numbers, core lists, set sizes

CONSTANTS
    Use,           \* the settings the program's setters may call (the others stay unset)
    MachineCount,  \* the cores of the machine (`available_parallelism`)
    MachineFails,  \* whether the machine may refuse a build whose settings are fine
    First,         \* what caller 1 calls: "build", "build_global", "init" or "default"
    Second,        \* what caller 2 calls, the same choices or "none"
    Mode           \* "ok", or the name of the one rule this run breaks (see the configurations)

Unset == -1                                                      \* a count never set
Callers == {1, 2}                                                \* the two callers
Op(c) == IF c = 1 THEN First ELSE Second                         \* what caller c calls
Min(m, n) == IF m < n THEN m ELSE n                              \* the smaller of two numbers
Max(m, n) == IF m > n THEN m ELSE n                              \* the larger of two numbers
When(p, s) == IF p THEN {s} ELSE {}                              \* s, when p holds
IsPow2(n) == \E k \in 0..15 : n = 2 ^ k                          \* `u16::is_power_of_two` (false for 0)

\* Every setting a setter records, by the name its error gives.
Settings == {"thread_stack_size", "max_nesting", "max_blocking_threads", "worker_threads",  \* the counts
             "max_worker_threads", "handoff_after", "on_long_poll", "event_interval",     \* and the knobs
             "global_queue_interval", "max_io_events_per_tick", "worker_cores",          \* the core lists
             "blocking_cores", "io_uring"}                                               \* the rings

\* The values a setter may give each setting.
Dom == [thread_stack_size |-> {"small", "enough"},               \* under 128 KiB, or at least it
        max_nesting |-> {0, 1},                                  \* no level, or one
        max_blocking_threads |-> {0, 1},                         \* no thread, or one
        worker_threads |-> {0, 2},                               \* no worker, or two
        max_worker_threads |-> {0, 1, 2},                        \* none, one or two at most
        handoff_after |-> {"none", "0.5ms", "10ms"},             \* never, under 1 ms, or 10 ms
        on_long_poll |-> {"set"},                                \* a hook given
        event_interval |-> {0, 1},                               \* 0 runs, or at least 1
        global_queue_interval |-> {0, 1},                        \* 0 runs, or at least 1
        max_io_events_per_tick |-> {0, 1},                       \* 0 events, or at least 1
        worker_cores |-> {"empty", "inside", "outside"},         \* a list of cores (see List)
        blocking_cores |-> {"empty", "inside", "outside"},       \* the same for the blocking pool
        io_uring |-> {"valid", "short_queue", "recv_not_pow2",   \* a ring's settings (see Io)
                      "recv_too_many", "small_buffer", "off_small_buffer"}]

\* A new builder (`Builder::new`): every setting unset.
Fresh == [thread_stack_size |-> "unset", max_nesting |-> Unset,  \* no stack size, no nesting limit
          max_blocking_threads |-> Unset, worker_threads |-> Unset,  \* no pool size, no count
          max_worker_threads |-> Unset, handoff_after |-> "unset",  \* no maximum, the default 10 ms
          on_long_poll |-> "unset", event_interval |-> Unset,    \* no hook, the default 61 runs
          global_queue_interval |-> Unset,                       \* the default 31 runs
          max_io_events_per_tick |-> Unset,                      \* every ready event
          worker_cores |-> "unset", blocking_cores |-> "unset",  \* no lists of cores
          io_uring |-> "unset"]                                  \* the process's ring settings

ProcessCores == 0..(MachineCount - 1)                            \* the cores the process may run on
List == [empty |-> <<>>,                                         \* no core at all
         inside |-> <<0>>,                                       \* one core the process may run on
         outside |-> <<0, MachineCount>>]                        \* naming a core it may not

\* The ring settings a program may give: queue entries, receive buffers, buffer bytes.
Io == [valid |-> [q |-> 64, r |-> 64, s |-> 16384],              \* every value fine
       short_queue |-> [q |-> 32, r |-> 32, s |-> 16384],        \* under 64 entries
       recv_not_pow2 |-> [q |-> 64, r |-> 48, s |-> 16384],      \* buffers not a power of two
       recv_too_many |-> [q |-> 64, r |-> 128, s |-> 16384],     \* not below twice the entries
       small_buffer |-> [q |-> 64, r |-> 64, s |-> 256],         \* buffers under 512 bytes
       off_small_buffer |-> [q |-> 64, r |-> 0, s |-> 256]]      \* the same, with receive buffers off
MinQueue == 64                                                   \* `MIN_QUEUE_LEN`
MinBuffer == 512                                                 \* `MIN_RECV_BUFFER_SIZE`
MaxBuffer == 2 ^ 30                                              \* `MAX_RECV_BUFFER_SIZE`, 1 GiB
MaxRecv == 32768                                                 \* the most receive buffers

Knobs == {"event_interval", "global_queue_interval", "max_io_events_per_tick"}  \* the knobs that asserted
Errors == {"invalid", "machine", "other"}                        \* the answers that are errors, AlreadyExists aside
InitMin == {Unset, 0, 2}                                         \* `with_min_threads`: unset, 0 or 2
InitMax == {Unset, 0, 1, 2}                                      \* `with_max_threads`: unset, 0, 1 or 2

\* Every mutation a configuration may name.
Modes == {"ok", "knob_asserts", "no_stack_min", "no_nesting_min", "no_blocking_min", "raise_max",  \* setters, ranges
          "no_handoff_min", "unset_as_none", "long_poll_with_none", "no_event_min",  \* the handoff, the knobs
          "no_queue_interval_min", "no_io_events_min", "no_queue_min", "no_recv_off",  \* and the ring
          "no_recv_rule", "no_buffer_rule", "empty_cores", "outside_cores",          \* the core lists
          "blocking_cores_unchecked", "wrong_name", "count_ignores_cores",           \* and the count
          "count_not_held", "global_other", "no_early_check", "keep_loser",          \* the default runtime
          "overwrite", "init_discards", "init_logs_exists"}                          \* and init

ASSUME Use \subseteq Settings /\ Mode \in Modes                  \* a configuration names known ones
ASSUME First \in {"build", "build_global", "init", "default"}    \* caller 1 makes a call
ASSUME Second \in {"build", "build_global", "init", "default", "none"}  \* caller 2 may make none
ASSUME MachineCount \in Nat /\ MachineCount > 0 /\ MachineFails \in BOOLEAN  \* a machine

(***************************************************************************)
(* What the documentation says: which settings are out of range, which do  *)
(* not fit together, and the workers a runtime starts with. The code's    *)
(* checks below are held to it.                                            *)
(***************************************************************************)

\* A list of cores that is empty or names a core the process may not run on.
ListFails(l) == l # "unset" /\ (List[l] = <<>> \/ \E i \in DOMAIN List[l] : List[l][i] \notin ProcessCores)

\* The ring settings out of range: at least 64 entries; receive buffers 0, or a power of two up to
\* 32768 below twice the entries; buffers from 512 bytes to 1 GiB, unless receive buffers are 0.
IoFails(c) ==
    When(c.q < MinQueue, "io_uring queue_len")                   \* too few entries
    \cup When(c.r # 0 /\ ~(IsPow2(c.r) /\ c.r <= MaxRecv /\ c.r < 2 * c.q), "io_uring recv_buffers")  \* bad count
    \cup When(c.r # 0 /\ (c.s < MinBuffer \/ c.s > MaxBuffer), "io_uring recv_buffer_size")  \* bad size

\* The settings out of range, each by its setter's documentation.
OutOfRange(x) ==
    When(x.thread_stack_size = "small", "thread_stack_size")     \* at least 128 KiB
    \cup When(x.max_nesting = 0, "max_nesting")                  \* at least 1
    \cup When(x.max_blocking_threads = 0, "max_blocking_threads")  \* at least 1
    \cup When(x.handoff_after = "0.5ms", "handoff_after")        \* at least 1 ms, or None
    \cup When(x.event_interval = 0, "event_interval")            \* at least 1
    \cup When(x.global_queue_interval = 0, "global_queue_interval")  \* at least 1
    \cup When(x.max_io_events_per_tick = 0, "max_io_events_per_tick")  \* at least 1
    \cup (IF x.io_uring = "unset" THEN {} ELSE IoFails(Io[x.io_uring]))  \* the ring's
    \cup When(ListFails(x.worker_cores), "worker_cores")         \* a list the process can run
    \cup When(ListFails(x.blocking_cores), "blocking_cores")     \* the same

\* The settings that do not fit with another, named as their documentation names them: a
\* max_worker_threads below an explicit worker_threads, an on_long_poll with handoff_after(None).
Misfit(x) ==
    When(x.worker_threads # Unset /\ x.max_worker_threads # Unset
         /\ x.max_worker_threads < x.worker_threads, "max_worker_threads")  \* too low a maximum
    \cup When(x.on_long_poll = "set" /\ x.handoff_after = "none", "on_long_poll")  \* nothing to pass

Failing(x) == OutOfRange(x) \cup Misfit(x)                       \* every setting that does not fit
Valid(x) == Failing(x) = {}                                      \* every setting fits

\* The workers an Ok runtime starts with: worker_threads; else one per core of worker_cores; else one
\* per core of the machine; either of the last two held to max_worker_threads when that is set.
BaseOf(x) == IF x.worker_cores = "unset" THEN MachineCount ELSE Len(List[x.worker_cores])  \* the cores'
SpecWorkers(x) ==
    IF x.worker_threads # Unset THEN x.worker_threads            \* the explicit count
    ELSE IF x.max_worker_threads = Unset THEN BaseOf(x)          \* the cores' count
    ELSE Min(BaseOf(x), x.max_worker_threads)                    \* held to the maximum

\* The most workers it may run over its life: max_worker_threads, else four times its start.
SpecCapacity(x) == IF x.max_worker_threads = Unset THEN 4 * SpecWorkers(x) ELSE x.max_worker_threads

(***************************************************************************)
(* What the code does.                                                      *)
(***************************************************************************)

\* `self.handoff_after.unwrap_or(Some(DEFAULT_HANDOFF))`: unset is the default 10 ms.
\* MUTATION unset_as_none: unset is read as None, so on_long_poll with no handoff_after is refused.
Handoff(x) == IF x.handoff_after # "unset" THEN x.handoff_after  \* as given
              ELSE IF Mode = "unset_as_none" THEN "none"         \* the planted bug
              ELSE "10ms"                                        \* the default

\* `IoUringConfiguration::validate`: the first ring setting refused, or "" for none, in its order.
\* MUTATION no_queue_min, no_recv_rule, no_buffer_rule: that check is gone.
\* MUTATION no_recv_off: the early return for receive buffers off is gone, so 0 buffers meets the
\* power-of-two rule, which refuses it.
IoRefusal(c) ==
    IF c.q < MinQueue /\ Mode # "no_queue_min" THEN "io_uring queue_len"  \* too few entries
    ELSE IF c.r = 0 /\ Mode # "no_recv_off" THEN ""              \* receive buffers off: nothing else checked
    ELSE IF ~(IsPow2(c.r) /\ c.r < 2 * c.q) /\ Mode # "no_recv_rule" THEN "io_uring recv_buffers"  \* bad count
    ELSE IF (c.s < MinBuffer \/ c.s > MaxBuffer) /\ Mode # "no_buffer_rule" THEN "io_uring recv_buffer_size"  \* bad size
    ELSE ""                                                      \* fine

\* `cores::validate`: an empty list, or one naming a core the process may not run on.
\* MUTATION empty_cores: an empty list is accepted. MUTATION outside_cores: so is a core outside.
ListRefused(l) ==
    /\ l # "unset"                                               \* a list was given
    /\ \/ List[l] = <<>> /\ Mode # "empty_cores"                 \* empty
       \/ \E i \in DOMAIN List[l] : List[l][i] \notin ProcessCores /\ Mode # "outside_cores"  \* a core outside

\* `Builder::validate`: the first setting refused, or "" when every one fits, in the code's order.
\* MUTATION no_stack_min, no_nesting_min, no_blocking_min, no_handoff_min, no_event_min,
\* no_queue_interval_min, no_io_events_min: that range check is gone.
\* MUTATION raise_max: no check of max_worker_threads against worker_threads (the old code, which
\* raised the maximum to the count instead).
\* MUTATION long_poll_with_none: on_long_poll is accepted with handoff_after(None).
\* MUTATION blocking_cores_unchecked: blocking_cores is not checked.
\* MUTATION wrong_name: blocking_cores is checked, but refused as worker_cores.
Refusal(x) ==
    IF x.thread_stack_size = "small" /\ Mode # "no_stack_min" THEN "thread_stack_size"  \* under 128 KiB
    ELSE IF x.max_nesting = 0 /\ Mode # "no_nesting_min" THEN "max_nesting"  \* under 1
    ELSE IF x.max_blocking_threads = 0 /\ Mode # "no_blocking_min" THEN "max_blocking_threads"  \* under 1
    ELSE IF x.worker_threads # Unset /\ x.max_worker_threads # Unset /\ x.max_worker_threads < x.worker_threads /\ Mode # "raise_max" THEN "max_worker_threads"  \* below the count
    ELSE IF Handoff(x) = "0.5ms" /\ Mode # "no_handoff_min" THEN "handoff_after"  \* under 1 ms
    ELSE IF x.on_long_poll = "set" /\ Handoff(x) = "none" /\ Mode # "long_poll_with_none" THEN "on_long_poll"  \* no watch
    ELSE IF x.event_interval = 0 /\ Mode # "no_event_min" THEN "event_interval"  \* under 1
    ELSE IF x.global_queue_interval = 0 /\ Mode # "no_queue_interval_min" THEN "global_queue_interval"  \* under 1
    ELSE IF x.max_io_events_per_tick = 0 /\ Mode # "no_io_events_min" THEN "max_io_events_per_tick"  \* under 1
    ELSE IF x.io_uring # "unset" /\ IoRefusal(Io[x.io_uring]) # "" THEN IoRefusal(Io[x.io_uring])  \* the ring's
    ELSE IF ListRefused(x.worker_cores) THEN "worker_cores"      \* the workers' cores
    ELSE IF ListRefused(x.blocking_cores) /\ Mode # "blocking_cores_unchecked"  \* the pool's cores,
         THEN IF Mode = "wrong_name" THEN "worker_cores" ELSE "blocking_cores"  \* named as such
    ELSE ""                                                      \* every setting fits

\* `Builder::worker_count`: the explicit count, else the list's length, else the machine's,
\* held to max_worker_threads when that is set.
\* MUTATION count_ignores_cores: the list's length is not used. MUTATION count_not_held: the count
\* is not held to max_worker_threads.
CodeWorkers(x) ==
    IF x.worker_threads # Unset THEN x.worker_threads            \* explicit
    ELSE LET base == IF x.worker_cores # "unset" /\ Mode # "count_ignores_cores"  \* the list's length,
                     THEN Len(List[x.worker_cores])
                     ELSE MachineCount                           \* or the machine's cores
         IN IF x.max_worker_threads # Unset /\ Mode # "count_not_held"  \* held to the maximum
            THEN Min(base, x.max_worker_threads)
            ELSE base                                            \* or not

\* `capacity` in `build`: max_worker_threads, else four times the count, and never below the count.
CodeCapacity(x) == Max(IF x.max_worker_threads # Unset THEN x.max_worker_threads ELSE 4 * CodeWorkers(x),
                       CodeWorkers(x))                           \* the `.max(workers)`

\* `GlobalExecutorConfig::builder`: a maximum of 0 starts no worker; any other starts the minimum
\* (at least one; one per core when unset) and allows the maximum (four times the minimum when
\* unset), never below the minimum.
ConfigBuilder(mn, mx) ==
    LET m == IF mn = Unset THEN MachineCount ELSE mn             \* the minimum, or one per core
        lo == Max(m, 1)                                          \* at least one worker
    IN IF mx = 0                                                 \* no worker at all
       THEN [Fresh EXCEPT !.worker_threads = 0]                  \* `worker_threads(0)`
       ELSE [Fresh EXCEPT !.worker_threads = lo,                 \* the minimum, and the maximum
                          !.max_worker_threads = Max(IF mx = Unset THEN 4 * lo ELSE mx, lo)]

VARIABLES
    pc,       \* each caller's next step
    b,        \* each caller's builder: the value of each setting
    built,    \* each caller's build: "none" (not run), "ok", "invalid", "machine" (refused by it)
    named,    \* the setting an InvalidInput names ("" otherwise)
    workers,  \* the workers an Ok runtime starts with
    cap,      \* the most workers it may run over its life
    ans,      \* each caller's answer: "none" yet, "ok", "invalid", "machine", "exists", "other"
    saw,      \* (record) the default runtime ran when the caller looked for it
    own,      \* the runtime the caller built: "none", "running", "dropped"
    logged,   \* the caller left a record at the error level
    global,   \* the caller whose runtime is the default, 0 for none (`DEFAULT`)
    ever      \* (record) every caller whose runtime was ever the default

vars == <<pc, b, built, named, workers, cap, ans, saw, own, logged, global, ever>>  \* every variable

Finish(c) == IF Op(c) = "init" THEN "report" ELSE "done"         \* init handles the answer it got

Init ==
    /\ pc = [c \in Callers |-> IF Op(c) = "none" THEN "done" ELSE "set"]  \* a caller with no call is done
    /\ b = [c \in Callers |-> Fresh]                             \* every builder new
    /\ built = [c \in Callers |-> "none"]                        \* nothing built
    /\ named = [c \in Callers |-> ""]                            \* nothing refused
    /\ workers = [c \in Callers |-> 0]                           \* no worker
    /\ cap = [c \in Callers |-> 0]                               \* and no room for one
    /\ ans = [c \in Callers |-> "none"]                          \* no answer yet
    /\ saw = [c \in Callers |-> FALSE]                           \* nobody looked
    /\ own = [c \in Callers |-> "none"]                          \* no runtime
    /\ logged = [c \in Callers |-> FALSE]                        \* nothing logged
    /\ global = 0                                                \* no default runtime
    /\ ever = {}                                                 \* and there never was one

(***************************************************************************)
(* The callers.                                                             *)
(***************************************************************************)

\* A setter records value v of setting s, the last call winning. None panics.
\* MUTATION knob_asserts: a knob's setter asserts its value is at least 1, as it did before.
Set(c, s, v) ==
    /\ pc[c] = "set"                                             \* the program is still setting
    /\ Op(c) \in {"build", "build_global"}                       \* on a builder of its own
    /\ IF Mode = "knob_asserts" /\ s \in Knobs /\ v = 0          \* the planted bug, and a 0 for a knob:
       THEN /\ pc' = [pc EXCEPT ![c] = "panicked"]               \* the setter panics
            /\ UNCHANGED b                                       \* nothing recorded
       ELSE /\ b' = [b EXCEPT ![c][s] = v]                       \* the value recorded
            /\ UNCHANGED pc                                      \* the program goes on
    /\ UNCHANGED <<built, named, workers, cap, ans, saw, own, logged, global, ever>>  \* nothing else

\* The caller makes its call. A build or build_global caller's builder is what its setters left;
\* init's is the one its executor configuration describes; try_default's has every setting at
\* its default.
Call(c) ==
    /\ pc[c] = "set"                                             \* not called yet
    /\ IF Op(c) = "init"                                         \* init with any configuration
       THEN \E mn \in InitMin, mx \in InitMax : b' = [b EXCEPT ![c] = ConfigBuilder(mn, mx)]
       ELSE UNCHANGED b                                          \* the others as they are
    /\ pc' = [pc EXCEPT ![c] = IF Op(c) = "build" THEN "build" ELSE "check"]  \* the global ones look first
    /\ UNCHANGED <<built, named, workers, cap, ans, saw, own, logged, global, ever>>  \* nothing else

\* build_global looks at the default first (`DEFAULT.get()`): running, it answers AlreadyExists and
\* builds nothing. try_default answers a running default as it is.
\* MUTATION global_other: build_global answers another error. MUTATION no_early_check: it does not
\* look, and builds anyway.
Check(c) ==
    /\ pc[c] = "check"                                           \* the caller looks
    /\ saw' = [saw EXCEPT ![c] = global # 0]                     \* what it would find
    /\ IF global # 0 /\ (Op(c) = "default" \/ Mode # "no_early_check")  \* the default runs
       THEN /\ ans' = [ans EXCEPT ![c] = IF Op(c) = "default" THEN "ok"  \* the free function uses it
                                         ELSE IF Mode = "global_other" THEN "other"  \* the planted bug
                                         ELSE "exists"]          \* AlreadyExists
            /\ pc' = [pc EXCEPT ![c] = Finish(c)]                \* the call is over
       ELSE /\ pc' = [pc EXCEPT ![c] = "build"]                  \* none: it builds
            /\ UNCHANGED ans                                     \* no answer yet
    /\ UNCHANGED <<b, built, named, workers, cap, own, logged, global, ever>>  \* nothing else

\* What a build can come to: InvalidInput when a setting is refused; else Ok, or the machine's
\* refusal where this run lets the machine refuse.
Outcomes(x) == IF Refusal(x) # "" THEN {"invalid"}               \* the configuration's mistake
               ELSE {"ok"} \cup (IF MachineFails THEN {"machine"} ELSE {})  \* the machine's

\* `Builder::build`: the settings checked, then the runtime started with its workers; a runtime
\* that cannot start drops (its started workers stop). build_global and the others go on to set
\* the default with an Ok one.
Build(c) ==
    /\ pc[c] = "build"                                           \* the caller builds
    /\ \E o \in Outcomes(b[c]) :                                 \* one of the outcomes
         /\ built' = [built EXCEPT ![c] = o]                     \* the build's
         /\ named' = [named EXCEPT ![c] = IF o = "invalid" THEN Refusal(b[c]) ELSE ""]  \* the setting named
         /\ workers' = [workers EXCEPT ![c] = IF o = "ok" THEN CodeWorkers(b[c]) ELSE 0]  \* `worker_count`
         /\ cap' = [cap EXCEPT ![c] = IF o = "ok" THEN CodeCapacity(b[c]) ELSE 0]  \* the capacity
         /\ own' = [own EXCEPT ![c] = IF o = "ok" THEN "running" ELSE "none"]  \* a runtime runs, or none
         /\ ans' = [ans EXCEPT ![c] = IF o = "ok" /\ Op(c) # "build" THEN "none" ELSE o]  \* build answers
         /\ pc' = [pc EXCEPT ![c] = IF o = "ok" /\ Op(c) # "build" THEN "publish" ELSE Finish(c)]  \* or sets
    /\ UNCHANGED <<b, saw, logged, global, ever>>                \* nothing else

\* build_global sets the default (`DEFAULT.set`): the runtime becomes it unless one already is;
\* then the call answers AlreadyExists and its runtime is dropped. try_default keeps one already
\* there (`get_or_init`) and answers it, its own runtime dropped.
\* MUTATION keep_loser: the runtime that lost stays running. MUTATION overwrite: build_global's set
\* replaces a default already there.
Publish(c) ==
    /\ pc[c] = "publish"                                         \* an Ok runtime to set
    /\ IF global = 0 \/ (Mode = "overwrite" /\ Op(c) # "default")  \* none yet, or the planted bug
       THEN /\ global' = c                                       \* it is the default
            /\ ever' = ever \cup {c}                             \* recorded
            /\ ans' = [ans EXCEPT ![c] = "ok"]                   \* Ok
            /\ UNCHANGED own                                     \* it runs
       ELSE /\ ans' = [ans EXCEPT ![c] = IF Op(c) = "default" THEN "ok" ELSE "exists"]  \* the other stays
            /\ own' = [own EXCEPT ![c] = IF Mode = "keep_loser" THEN "running" ELSE "dropped"]  \* this one goes
            /\ UNCHANGED <<global, ever>>                        \* the default as it was
    /\ pc' = [pc EXCEPT ![c] = Finish(c)]                        \* the call is over
    /\ UNCHANGED <<b, built, named, workers, cap, saw, logged>>  \* nothing else

\* init_with_config handles build_global's answer: AlreadyExists is silent, Ok is the default
\* started, and any other error is logged at the error level (`log::error!`).
\* MUTATION init_discards: the error is dropped unlogged, as the code did before.
\* MUTATION init_logs_exists: AlreadyExists is logged too.
Report(c) ==
    /\ pc[c] = "report"                                          \* init has its answer
    /\ logged' = [logged EXCEPT ![c] = (ans[c] \in Errors /\ Mode # "init_discards")  \* an error
                                       \/ (ans[c] = "exists" /\ Mode = "init_logs_exists")]  \* the planted bug
    /\ pc' = [pc EXCEPT ![c] = "done"]                           \* init returns
    /\ UNCHANGED <<b, built, named, workers, cap, ans, saw, own, global, ever>>  \* nothing else

\* Every call is over: nothing more happens.
Rest ==
    /\ \A c \in Callers : pc[c] \in {"done", "panicked"}         \* every caller finished or panicked
    /\ UNCHANGED vars                                            \* and stays so

Next ==
    \/ \E c \in Callers :                                        \* a caller's step
          \/ \E s \in Use : \E v \in Dom[s] : Set(c, s, v)       \* a setter call
          \/ Call(c) \/ Check(c) \/ Build(c) \/ Publish(c) \/ Report(c)  \* or the call's steps
    \/ Rest                                                      \* or nothing left

Spec == Init /\ [][Next]_vars                                    \* every behaviour; only safety is checked

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* No setter call ever panics.
SettersNeverPanic == \A c \in Callers : pc[c] # "panicked"

\* A build answers Ok exactly when every setting is in range and they fit together: InvalidInput
\* when one does not, and with every one fine Ok, or the machine's refusal where it may refuse.
BuildOkIffValid == \A c \in Callers : built[c] # "none" => (built[c] = "invalid" <=> ~Valid(b[c]))

\* An InvalidInput names a setting that is in fact out of range, or does not fit with another.
ErrorNamesFailingSetting == \A c \in Callers : built[c] = "invalid" => named[c] \in Failing(b[c])

\* An Ok runtime starts with the documented count and may run at most max_worker_threads.
WorkerCount ==
    \A c \in Callers : built[c] = "ok" =>                        \* every Ok runtime
        /\ workers[c] = SpecWorkers(b[c])                        \* starts with the documented count
        /\ cap[c] = SpecCapacity(b[c])                           \* has the documented room
        /\ workers[c] <= cap[c]                                  \* which holds its start
        /\ b[c].max_worker_threads # Unset => cap[c] <= b[c].max_worker_threads  \* within the maximum

\* A build_global that finds the default running answers AlreadyExists and builds nothing; one
\* that loses the race to set it answers AlreadyExists too, and no runtime of its own runs.
SecondGlobalIsAlreadyExists ==
    \A c \in Callers : Op(c) \in {"build_global", "init"} =>     \* every call that sets the default
        /\ saw[c] => ans[c] \in {"none", "exists"} /\ built[c] = "none"  \* found it: AlreadyExists, no build
        /\ ans[c] = "exists" => own[c] # "running"               \* lost the race: its runtime dropped

\* The default runtime, once set, is never replaced.
OneDefault == Cardinality(ever) <= 1

\* An init whose build failed for a reason other than AlreadyExists leaves an error-level record,
\* and no default of its own; one that answered Ok or AlreadyExists logs nothing.
InitReportsFailure ==
    \A c \in Callers : Op(c) = "init" /\ pc[c] = "done" =>       \* every init that returned
        /\ ans[c] \in Errors => logged[c] /\ global # c          \* a failure: logged, no default
        /\ ans[c] \in {"ok", "exists"} => ~logged[c]             \* else silent

\* The builder an executor configuration describes always fits: init fails only by the machine.
InitConfigFits == \A c \in Callers : Op(c) = "init" /\ built[c] # "none" => Valid(b[c])

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A build_global loses the race to set the default, after it built.
NoLostRace == \A c \in Callers : ~(ans[c] = "exists" /\ own[c] = "dropped")
\* A runtime starts with fewer workers than its cores give, held to max_worker_threads.
NoHeldCount == \A c \in Callers : built[c] = "ok" /\ b[c].worker_threads = Unset => workers[c] = BaseOf(b[c])
\* A runtime starts with no worker at all.
NoZeroWorkers == \A c \in Callers : built[c] = "ok" => workers[c] > 0
\* An init logs its failure.
NoInitLog == \A c \in Callers : ~logged[c]
\* A build is refused while two settings fail at once (it names one of them).
NoDoubleFailure == \A c \in Callers : built[c] = "invalid" => Cardinality(Failing(b[c])) = 1
\* A buffer size out of range is accepted, as receive buffers are off.
NoOffBuffer == \A c \in Callers : built[c] = "ok" => b[c].io_uring # "off_small_buffer"
=============================================================================

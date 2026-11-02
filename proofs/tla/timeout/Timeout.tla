------------------------------- MODULE Timeout -------------------------------
(***************************************************************************)
(* A timeout (src/time/timeout.rs: `timeout`, `timeout_at`, `Future for    *)
(* Timeout`): a future raced against a sleep, on a clock that moves on.    *)
(*                                                                          *)
(* In plain words: `timeout` takes its deadline once, as it is made: now   *)
(* plus the duration, or the far future when that is past the clock's     *)
(* range. Each poll polls the inner future first, so a future that is      *)
(* ready wins whatever the clock says; only a future still pending lets    *)
(* the sleep be polled, which answers Elapsed once the deadline passed and *)
(* otherwise registers the task's waker with the timer. Both polls take    *)
(* the same waker, so the task is woken by whichever comes first: the      *)
(* inner future's event, or the deadline.                                  *)
(*                                                                          *)
(* Who acts: the clock, the inner future's source (which makes it ready    *)
(* at a moment of its choosing, or never), and the task that awaits the    *)
(* timeout. What must never happen: Elapsed answered while the inner       *)
(* future was ready at that poll; a deadline                               *)
(* computed past the clock's range (a panic in the code); a task left      *)
(* asleep with its future ready or its deadline passed.                    *)
(*                                                                          *)
(* What is abstract: time is whole units; the sleep fires at its deadline  *)
(* (the timer families check how) and the inner future's wake is one flag. *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Duration,  \* the timeout's duration, in units
    ReadyAt,   \* when the inner future becomes ready (past Horizon: never)
    MaxStep,   \* the most the clock moves on in one step
    Horizon,   \* the clock stops here
    Range,     \* the last instant the clock can name; the far future is this instant
    Mutation   \* "none", or the name of the one rule this run takes away

\* The far future: the last instant the clock can name.
Far == Range

(* --algorithm Timeout
variables
    now = 0,                 \* the clock
    deadline = 0,            \* the timeout's deadline, taken as it is made
    ready = FALSE,           \* the inner future is ready
    futRegistered = FALSE,   \* the inner future holds the task's waker
    timerRegistered = FALSE, \* the timer holds the task's waker
    woken = TRUE,            \* the task is to be polled (the first poll is due)
    result = "none",         \* (record) "ok", "elapsed", or "none" yet
    readyLost = FALSE,       \* (record) Elapsed answered with the future ready at that poll
    pastRange = FALSE;       \* (record) a deadline past the clock's range

define
    \* A ready future wins.
    ReadyWins == ~readyLost
    \* The deadline is one the clock can name.
    InRange == ~pastRange
    \* The timeout completes: the future is ready in time, or the deadline comes, within
    \* the horizon.
    Ends == <>(result # "none")
end define;

\* The clock moves on; a timer whose deadline passed wakes the task it holds.
fair process clock = "clock"
begin
C_tick:
    while now < Horizon do
        with step \in 1..MaxStep do
            now := IF now + step > Horizon THEN Horizon ELSE now + step;
        end with;
        if timerRegistered /\ now >= deadline then
            timerRegistered := FALSE;
            woken := TRUE;
        end if;
    end while;
end process;

\* The inner future's source makes it ready at ReadyAt, and wakes the waker it holds.
fair process source = "source"
begin
F_ready:
    await now >= ReadyAt;
    ready := TRUE;
    if futRegistered then
        futRegistered := FALSE;
        woken := TRUE;
    end if;
end process;

\* The task: makes the timeout, then polls it each time it is woken.
fair process task = "task"
begin
T_make:
    \* `Instant::now().checked_add(duration).unwrap_or_else(far_future)`.
    \* MUTATION timeout_unchecked: the sum as it is (a panic past the range in the code).
    if now + Duration > Range /\ Mutation # "timeout_unchecked" then
        deadline := Far;
    else
        deadline := now + Duration;
        pastRange := now + Duration > Range;
    end if;
T_poll:
    \* Woken (or the first poll): the inner future first, then the sleep.
    \* MUTATION sleep_first: the sleep is polled first.
    await woken;
    woken := FALSE;
    if Mutation = "sleep_first" /\ now >= deadline then
        readyLost := ready;        \* (record) a ready future lost to the clock
        result := "elapsed";
        goto Done;
    elsif ready then
        result := "ok";
        goto Done;
    else
        futRegistered := TRUE;
    end if;
T_sleep:
    \* The future is pending: the sleep answers Elapsed past the deadline, or registers.
    \* MUTATION no_sleep_poll: a pending future leaves the sleep unpolled, and unarmed.
    if Mutation = "no_sleep_poll" then
        goto T_poll;
    elsif now >= deadline then
        result := "elapsed";       \* the future was pending at this poll's first look
        goto Done;
    else
        timerRegistered := TRUE;
        goto T_poll;
    end if;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

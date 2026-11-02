----------------------------- MODULE ForeignArm -----------------------------
(***************************************************************************)
(* A sleep armed from a thread that runs no shard of the runtime, while    *)
(* its clock is paused (src/time/sleep.rs: `Timer::after`, which reads the *)
(* clock for the deadline, and `Timer::poll_remote`, which posts the arm   *)
(* request; src/rt/paused.rs: `attempt`, the jump). A witness of an open   *)
(* question, not fixed: what today's code does.                            *)
(*                                                                          *)
(* In plain words: a thread outside the runtime makes a sleep, reading the *)
(* paused clock for its deadline, and later polls it, which posts the arm  *)
(* request to a worker shard. The runtime counts only its own shards and   *)
(* its blocking jobs: between the read and the post nothing tells it the   *)
(* thread is about to arm a timer. A worker parked with a later timer      *)
(* finds every shard idle and moves the clock to that timer, past the      *)
(* foreign sleep's deadline; the foreign sleep then ends at once, later on *)
(* the paused clock than it was due.                                       *)
(*                                                                          *)
(* What must never happen (and does here): an armed timer behind the       *)
(* clock, a sleep that ends later on the paused clock than its deadline.   *)
(*                                                                          *)
(* What is abstract: time is a count; the worker is parked and idle from   *)
(* the start with its own timer at 2, so its attempt is one step: it looks *)
(* (every shard idle: nothing posted to it) and jumps; the shard's apply   *)
(* of the arm request is left out, as the request lands behind the clock   *)
(* whenever it is applied.                                                 *)
(***************************************************************************)
EXTENDS Integers, TLC

\* No deadline: past every time the model reaches.
Inf == 1000

(* --algorithm ForeignArm
variables
    clock = 0,          \* the paused clock
    workerAt = 2,       \* the parked worker's own timer, published
    foreignAt = Inf,    \* the foreign sleep's deadline, once its arm request is posted
    posted = FALSE;     \* the arm request is in the worker's mailbox: the worker is not idle

define
    \* No armed timer is behind the clock: the foreign sleep, once armed, is never passed.
    ForeignNeverPassed == clock <= foreignAt
end define;

fair process Foreign = "foreign"
variable at = 0;
begin
F_make: \* `sleep(1)`: the deadline is the clock now, plus one
    at := clock + 1;
F_post: \* the poll posts the arm request to the worker
    foreignAt := at || posted := TRUE;
end process;

fair process Worker = "worker"
begin
W_try: \* parked, the worker tries the jump: every shard idle (nothing posted), so it moves the
       \* clock to the earliest deadline it was shown, its own
    if ~posted then
        clock := workerAt;
    end if;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

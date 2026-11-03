----------------------------- MODULE ForeignArm -----------------------------
(***************************************************************************)
(* A sleep armed from a thread that runs no shard of the runtime, while    *)
(* its clock is paused (src/time/sleep.rs: `Timer::after`, which reads the *)
(* clock for the deadline, and `Timer::poll_remote`, which posts the arm   *)
(* request under the jump protocol; src/time/clock.rs: `Clock::wait_advance`*)
(* and the count of arms in flight; src/rt/paused.rs: `attempt`, the jump, *)
(* which counts those arms as work in flight).                             *)
(*                                                                          *)
(* In plain words: a thread outside the runtime (or one whose shard the    *)
(* handoff took) makes a sleep, reading the paused clock for its deadline, *)
(* and later polls it. The poll counts itself in flight, waits for a jump  *)
(* being decided to end, looks at the clock (a deadline already passed     *)
(* ends the sleep at once), posts the arm request to a worker shard (which *)
(* holds it, and so is not idle), and only then counts itself out. A      *)
(* worker parked with a later timer decides a jump under the clock's      *)
(* advance claim, and only when nothing is in flight.                      *)
(*                                                                          *)
(* What must never happen: an armed timer behind the clock: the jump past  *)
(* a deadline the sleep was about to arm.                                  *)
(*                                                                          *)
(* What is abstract: time is a count; the worker is parked and idle from   *)
(* the start with its own timer at 2, so its attempt is one claim and one  *)
(* look; the shard's apply of the arm request is left out (the request     *)
(* makes it not idle until then).                                          *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Mutation   \* "none", or the name of the one rule this run takes away

\* No deadline: past every time the model reaches.
Inf == 1000

(* --algorithm ForeignArm
variables
    clock = 0,          \* the paused clock
    workerAt = 2,       \* the parked worker's own timer, published
    foreignAt = Inf,    \* the foreign sleep's deadline, once its arm request is posted
    posted = FALSE,     \* the arm request is in the worker's mailbox: the worker is not idle
    inFlight = FALSE,   \* the foreign poll counts itself in flight
    advancing = FALSE,  \* the clock's advance claim
    ended = FALSE;      \* the foreign sleep ended at once, its deadline passed

define
    \* No armed timer is behind the clock: the foreign sleep, once armed, is never passed.
    ForeignNeverPassed == clock <= foreignAt
end define;

fair process Foreign = "foreign"
variable at = 0;
begin
F_make:
    \* `sleep(1)`: the deadline is the clock now, plus one.
    at := clock + 1;
F_mark:
    \* The poll counts itself in flight. MUTATION no_in_flight: it does not.
    inFlight := Mutation # "no_in_flight";
F_wait:
    \* `wait_advance`: a jump being decided ends first. MUTATION no_wait_advance: it does not wait.
    await ~advancing \/ Mutation = "no_wait_advance";
F_check:
    \* A deadline already passed ends the sleep at once.
    if clock >= at then
        ended := TRUE;
        goto F_out;
    end if;
F_post:
    \* The arm request goes to the worker.
    foreignAt := at || posted := TRUE;
F_out:
    \* Counted out of flight.
    inFlight := FALSE;
end process;

fair process Worker = "worker"
begin
W_claim:
    \* Parked, the worker tries the jump under the advance claim.
    await ~advancing;
    advancing := TRUE;
W_look:
    \* Its last look: every shard idle (nothing posted to it) and nothing in flight, or no jump.
    if posted \/ inFlight then
        advancing := FALSE;
        goto Done;
    end if;
W_jump:
    \* It moves the clock to the earliest deadline it was shown, its own, then lets the claim go.
    clock := workerAt;
W_release:
    advancing := FALSE;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

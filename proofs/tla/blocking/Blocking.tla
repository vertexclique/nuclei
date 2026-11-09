------------------------------- MODULE Blocking -------------------------------
(***************************************************************************)
(* A blocking wait on a thread that runs no shard (src/sync/internal.rs:   *)
(* `block_on`, `block_on_until`, `Unpark`, `wait_here`, and the last       *)
(* branches of `blocking` and `blocking_until`), at the grain of each      *)
(* poll, park and unpark.                                                  *)
(*                                                                          *)
(* In plain words: the thread polls the future with a waker that unparks   *)
(* it. Pending, it parks. The thread's park token keeps an unpark that     *)
(* came before the park, so that park returns at once; a park may also    *)
(* return for no reason. Every return polls again. Another thread makes    *)
(* the future ready and calls the waker it registered. Before each park   *)
(* the thread submits what its polls retired to kovan                      *)
(* (`kovan::flush_pending`), so a sleeping thread holds nothing back.      *)
(* With a deadline (`block_on_until`, under every `blocking_recv_deadline`) *)
(* each pending poll is followed by a look at the clock: the deadline     *)
(* reached answers None, else the park lasts until the deadline at most,  *)
(* and every return polls again first. On wasm32 there is                 *)
(* no other thread: a future not ready at its one poll would wait for this *)
(* same thread, so `wait_here` answers an error instead of parking (and   *)
(* `blocking` panics with it).                                             *)
(*                                                                          *)
(* Who acts: the waiting thread, and (off wasm) the thread that releases   *)
(* what it waits for. What must never happen: the waiting thread parked   *)
(* for good with its future ready; an error off wasm; a wait that parks    *)
(* the only thread on wasm; a timeout before the deadline, or one that no *)
(* poll after the last park preceded; a sleep with an unsubmitted batch.   *)
(*                                                                          *)
(* What is abstract: the future is one flag and one waker slot (the       *)
(* primitives' own families check their register-then-look handshakes);   *)
(* the branches of `blocking` that run on a shard (a nested `block_on`, or *)
(* the thread's own shard) are the runtime's models'.                     *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Wasm,      \* the target is wasm32: one thread, no releaser
    ReadyAtOnce, \* the future is ready at the first poll
    Spurious,  \* how many times a park may return for no reason
    Deadline,  \* the wait has a deadline (`block_on_until`; never on wasm32)
    Retires,   \* a pending poll retires into the thread's kovan batch (a channel's receive)
    Mutation   \* "none", or the name of the one rule this run takes away

WaiterT == "waiter"
ReleaserT == "releaser"
ClockT == "clock"

(* --algorithm Blocking
variables
    ready = ReadyAtOnce,     \* the future would answer Ready
    registered = FALSE,      \* the future holds the thread's waker
    token = FALSE,           \* the thread's park token
    parked = FALSE,          \* the thread is parked
    spurious = 0,            \* spurious returns used
    past = FALSE,            \* the deadline passed
    batch = FALSE,           \* the thread holds retired values not submitted to kovan
    parks = 0,               \* (record) the park returns so far
    polled = TRUE,           \* (record) a poll came after the last park's return
    result = "none";         \* (record) "ready", "error", "timeout", or "none" yet

define
    \* Off wasm the wait never answers an error.
    NoErrorOffWasm == ~Wasm => result # "error"
    \* On wasm a future pending at once answers an error, and nothing parks.
    WasmNeverParks == Wasm => ~parked
    \* A deadline wait answers None only once the deadline passed.
    NeverEarly == result = "timeout" => past
    \* ... and only right after a poll that came after the last park returned.
    FinalPoll == result = "timeout" => polled
    \* A parked thread holds no retired value back.
    SleepsHoldingNothing == parked => ~batch
    \* The wait ends.
    Ends == <>(result # "none")
end define;

fair process waiter = WaiterT
begin
W_poll:
    \* One poll with the thread's waker: Ready, or the waker registered (and what the poll
    \* retired kept in the thread's batch).
    polled := TRUE;
    if ready then
        result := "ready";
        goto Done;
    else
        registered := TRUE;
        if Retires then batch := TRUE; end if;
    end if;
W_after:
    \* `wait_here` on wasm: a pending future is an error, not a park.
    \* MUTATION wasm_parks: wasm parks the thread as elsewhere.
    if Wasm /\ Mutation # "wasm_parks" then
        result := "error";
        goto Done;
    end if;
W_due:
    \* `block_on_until`: `checked_duration_since(now)?` and `left.is_zero()`, after the poll.
    \* MUTATION early_timeout: a park's return is taken for the deadline.
    if Deadline /\ (past \/ (Mutation = "early_timeout" /\ parks > 0)) then
        result := "timeout";
        goto Done;
    end if;
W_flush:
    \* `kovan::flush_pending()`: the batch submitted before the park.
    \* MUTATION no_flush: the thread parks holding its batch.
    if Mutation # "no_flush" then batch := FALSE; end if;
W_park:
    \* `thread::park` (`park_timeout(left)` with a deadline): a kept unpark returns at once;
    \* else the thread sleeps until one, the deadline, or a return for no reason.
    if token then
        token := FALSE;
        goto W_poll;
    else
        parked := TRUE;
    end if;
W_sleep:
    either
        await token;
        token := FALSE;
    or
        await spurious < Spurious;
        spurious := spurious + 1;
    or
        await Deadline /\ past;
    end either;
    parked := FALSE;
    parks := parks + 1;
    polled := FALSE;
W_woke:
    \* Every return polls again first.
    \* MUTATION no_final_poll: a park that returns past the deadline answers None unpolled.
    if Mutation = "no_final_poll" /\ Deadline /\ past then
        result := "timeout";
        goto Done;
    else
        goto W_poll;
    end if;
end process;

\* With a deadline: the clock passes it, once, at any point.
fair process clock \in (IF Deadline THEN {ClockT} ELSE {})
begin
C_pass:
    past := TRUE;
end process;

\* Off wasm: another thread makes the future ready and calls the waker it holds.
fair process releaser \in (IF Wasm THEN {} ELSE {ReleaserT})
begin
R_ready:
    ready := TRUE;
R_wake:
    \* `Unpark::wake`: `unpark` sets the token, waking the thread if it sleeps.
    \* MUTATION unpark_lost: an unpark while the thread is not parked is dropped.
    if registered then
        registered := FALSE;
        if parked \/ Mutation # "unpark_lost" then
            token := TRUE;
        end if;
    end if;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

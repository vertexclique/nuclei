------------------------------- MODULE Blocking -------------------------------
(***************************************************************************)
(* A blocking wait on a thread that runs no shard (src/sync/internal.rs:   *)
(* `block_on`, `Unpark`, `wait_here`, and the last branch of `blocking`),  *)
(* at the grain of each poll, park and unpark.                             *)
(*                                                                          *)
(* In plain words: the thread polls the future with a waker that unparks   *)
(* it. Pending, it parks. The thread's park token keeps an unpark that     *)
(* came before the park, so that park returns at once; a park may also    *)
(* return for no reason. Every return polls again. Another thread makes    *)
(* the future ready and calls the waker it registered. On wasm32 there is *)
(* no other thread: a future not ready at its one poll would wait for this *)
(* same thread, so `wait_here` answers an error instead of parking (and   *)
(* `blocking` panics with it).                                             *)
(*                                                                          *)
(* Who acts: the waiting thread, and (off wasm) the thread that releases   *)
(* what it waits for. What must never happen: the waiting thread parked   *)
(* for good with its future ready; an error off wasm; a wait that parks    *)
(* the only thread on wasm.                                                *)
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
    Mutation   \* "none", or the name of the one rule this run takes away

WaiterT == "waiter"
ReleaserT == "releaser"

(* --algorithm Blocking
variables
    ready = ReadyAtOnce,     \* the future would answer Ready
    registered = FALSE,      \* the future holds the thread's waker
    token = FALSE,           \* the thread's park token
    parked = FALSE,          \* the thread is parked
    spurious = 0,            \* spurious returns used
    result = "none";         \* (record) "ready", "error", or "none" yet

define
    \* Off wasm the wait never answers an error.
    NoErrorOffWasm == ~Wasm => result # "error"
    \* On wasm a future pending at once answers an error, and nothing parks.
    WasmNeverParks == Wasm => ~parked
    \* The wait ends.
    Ends == <>(result # "none")
end define;

fair process waiter = WaiterT
begin
W_poll:
    \* One poll with the thread's waker: Ready, or the waker registered.
    if ready then
        result := "ready";
        goto Done;
    else
        registered := TRUE;
    end if;
W_after:
    \* `wait_here` on wasm: a pending future is an error, not a park.
    \* MUTATION wasm_parks: wasm parks the thread as elsewhere.
    if Wasm /\ Mutation # "wasm_parks" then
        result := "error";
        goto Done;
    end if;
W_park:
    \* `thread::park`: a kept unpark returns at once; else the thread sleeps until one, or
    \* returns for no reason.
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
    end either;
    parked := FALSE;
    goto W_poll;
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

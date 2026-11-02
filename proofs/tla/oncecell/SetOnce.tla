------------------------------- MODULE SetOnce -------------------------------
(***************************************************************************)
(* SetOnce (src/sync/set_once.rs: `SetOnce::set`, `SetOnce::wait`), a      *)
(* once-cell paired with an event, at the grain of the steps the two       *)
(* compose.                                                                *)
(*                                                                          *)
(* In plain words: `set` asks the cell to take the value (`OnceCell::set`):*)
(* it refuses if the cell holds a value, takes the turn only if it is free *)
(* at once (a set racing another set or an initializer is refused, its     *)
(* value given back), writes the slot, publishes it with one               *)
(* compare-and-swap and closes the turn; only then does it set the event,  *)
(* which wakes every waiter. `wait` loops: it answers the value if the     *)
(* cell holds one, else it waits for the event and looks again.            *)
(*                                                                          *)
(* Who acts: the setters (each one `set`) and the waiters (each one        *)
(* `wait`). What must never happen: two sets both answered Ok; a wait that *)
(* ends without the value; the event set while the cell is empty (a       *)
(* waiter it wakes would look, find nothing and wait on a set event, a     *)
(* spin); a waiter left asleep once a set succeeded.                       *)
(*                                                                          *)
(* What is abstract: the event (src/sync/event.rs) is a flag and a set of  *)
(* queued waiters that its set grants all at once, and a wait that queues  *)
(* after the set is granted by its own pass (the event family checks that  *)
(* protocol step by step); the turn is one permit (the semaphore family's  *)
(* part); the cell's value pointer is null or published.                   *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Setters,   \* the threads that call `set`, one call each
    Waiters,   \* the threads that call `wait`, one call each
    Mutation   \* "none", or the name of the one rule this run takes away

(* --algorithm SetOnce
variables
    published = FALSE,                  \* the value pointer: null, or the slot published
    permit = TRUE,                      \* the turn to write the slot is free
    closed = FALSE,                     \* the turn queue is closed (a value is there)
    evSet = FALSE,                      \* the event's SET bit
    queued = {},                        \* waiters queued on the event, not yet granted
    granted = {},                       \* waiters the event granted, not yet woken up
    okSets = 0,                         \* (record) sets that answered Ok
    answered = {};                      \* (record) waiters whose wait answered the value

define
    \* At most one set takes the cell.
    OneOk == okSets <= 1
    \* The event is set only once the value is published: a waiter it wakes finds it.
    EventMeansValue == evSet => published
    \* Every waiter ends with the value. (The first set to reach the turn always takes it
    \* and publishes, so some set succeeds in every run.)
    WaitsEnd == <>(answered = Waiters)
end define;

\* `Event::set`: the bit goes up and every queued waiter is granted.
macro setEvent() begin
    evSet := TRUE;
    granted := granted \cup queued;
    queued := {};
end macro;

fair process setter \in Setters
begin
S_look:
    \* `OnceCell::set`: a cell that holds a value refuses at once.
    if published then
        goto Done;
    end if;
S_turn:
    \* `try_acquire_permit(1)`: the turn only if it is free now; a closed queue, or a turn
    \* somebody holds, refuses (AlreadyInitializedError, InitializingError).
    if permit /\ ~closed then
        permit := FALSE;
    else
        goto Done;
    end if;
S_early:
    \* MUTATION event_before_publish: the event is set before the value is published.
    if Mutation = "event_before_publish" then
        setEvent();
    end if;
S_publish:
    \* `publish_inline`: the slot written and published with one compare-and-swap (no racing
    \* builder here, so it wins), the turn kept for good and the queue closed.
    published := TRUE;
    closed := TRUE;
    okSets := okSets + 1;
S_event:
    \* Then the event, which wakes every waiter.
    \* MUTATION no_event: the event is never set.
    if Mutation \notin {"no_event", "event_before_publish"} then
        setEvent();
    end if;
end process;

fair process waiter \in Waiters
begin
W_get:
    \* `self.cell.get()`: the value, if published.
    if published then
        answered := answered \cup {self};
        goto Done;
    end if;
W_wait:
    \* `EventWait::poll`: a set event answers at once; else the waiter queues, and the
    \* pass its enqueue runs grants it if the event is set by then.
    if evSet then
        goto W_get;
    else
        queued := queued \cup {self};
    end if;
W_sleep:
    \* Asleep until the event grants it; then the loop looks at the cell again.
    await self \in granted;
    granted := granted \ {self};
    queued := queued \ {self};
    goto W_get;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

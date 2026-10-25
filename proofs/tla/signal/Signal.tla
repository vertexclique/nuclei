------------------------------- MODULE Signal -------------------------------
(***************************************************************************)
(* A unix signal's deliveries and a listener of it (src/signal/unix.rs:    *)
(* `on_signal`, `pump`; src/signal/mod.rs: `delivered`, `dispatch`,       *)
(* `Listener::poll_recv`), at the grain of each atomic access.            *)
(*                                                                          *)
(* In plain words: the handler raises the signal's count, then writes to  *)
(* the process's wake descriptor. Each pump (one per shard with a         *)
(* listener) waits for the descriptor, drains it, then dispatches: it     *)
(* loads the count, swaps it into the count the last dispatch woke for,   *)
(* and if that moved wakes the listener's waker, which a wake takes. The  *)
(* listener takes an item when the count moved since it last looked;     *)
(* else it registers its waker, looks once more, and only then sleeps     *)
(* until woken. It must see every delivery, coalesced or not, however the *)
(* steps of the handler, the pumps and itself fall.                        *)
(*                                                                          *)
(* What is abstract: the descriptor is a count of writes not drained (an  *)
(* eventfd's counter; a pipe's bytes), and the listener's waker is a flag *)
(* (`AtomicWaker`: a wake takes it).                                        *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    N,         \* deliveries of the signal
    Pumps,     \* the pumps, numbered 1, 2, ...
    Mutation   \* "none", or the name of the one rule this run takes away

HandlerT == 0
ListenerT == -1

(* --algorithm Signal
variables
    count = 0,        \* deliveries counted
    wake = 0,         \* writes to the wake descriptor not drained
    dispatched = 0,   \* the count the last dispatch woke for
    waker = FALSE,    \* the listener's waker is registered
    woken = FALSE,    \* a wake came for the sleeping listener
    seen = 0,         \* the count the listener last saw
    items = 0,        \* (record) items the listener took
    everWoken = FALSE;\* (record) a pump's wake woke the listener

fair process handler = HandlerT
begin
H_loop:
    while count < N do
        \* MUTATION write_first: the handler writes before it counts.
        if Mutation = "write_first" then
            wake := wake + 1;
        else
            count := count + 1;
        end if;
H_second:
        if Mutation = "write_first" then
            count := count + 1;
        else
            wake := wake + 1;
        end if;
    end while;
end process;

fair process pump \in Pumps
variables now = 0, moved = FALSE;
begin
P_loop:
    while TRUE do
        await wake > 0;
        \* MUTATION dispatch_first: the pump looks at the count before it drains.
        if Mutation = "dispatch_first" then
            now := count;
        else
            wake := 0;
        end if;
P_load:
        if Mutation = "dispatch_first" then
            wake := 0;
        else
            now := count;
        end if;
P_swap:
        moved := dispatched # now;
        dispatched := now;
P_wake:
        if moved /\ waker then
            waker := FALSE;
            woken := TRUE;
            everWoken := TRUE;
        end if;
    end while;
end process;

fair process listener = ListenerT
begin
L_check:
    while seen < N do
        if count # seen then
            seen := count;
            items := items + 1;
        else
L_register:
            waker := TRUE;
L_recheck:
            \* MUTATION no_recheck: the listener sleeps without looking again.
            if count # seen /\ Mutation # "no_recheck" then
                seen := count;
                items := items + 1;
            else
L_sleep:
                await woken;
                woken := FALSE;
            end if;
        end if;
    end while;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* The listener sees every delivery.
SeesAll == <>(seen = N)

\* It never takes more items than there were deliveries.
NoExtraItem == items <= N

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* Deliveries come as one item: the listener saw them all in fewer items.
NoCoalesce == ~(seen = N /\ items < N)
\* A pump's wake ends the listener's sleep.
NoPumpWake == ~everWoken
=============================================================================

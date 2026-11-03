----------------------------- MODULE SignalPump -----------------------------
(***************************************************************************)
(* The pumps that serve a unix signal's listeners, as they start and go    *)
(* (src/signal/unix.rs: `ensure_pump`, `pump`, `Pumping`; the listener's  *)
(* `Listener::poll_recv` in src/signal/mod.rs).                            *)
(*                                                                          *)
(* In plain words: a listener that waits registers its waker, then makes   *)
(* sure a pump serves it: a flag of the place it is polled on (the shard   *)
(* that runs it, `Shared::signal_pump`; or, where no shard runs, the one   *)
(* flag of the pump on a runtime worker) says whether one does, and the   *)
(* listener that raises it starts one. Then it looks at the count once     *)
(* more and sleeps. A pump waits for the wake descriptor, drains it,       *)
(* dispatches, and waits again, for good: but its task can be dropped      *)
(* without ever ending, when the shard that runs it goes (its runtime is  *)
(* dropped). The pump's drop lowers the flag, then wakes every listener,  *)
(* so each polls again and starts a pump where none serves it any more.    *)
(* No delivery may then go unseen.                                         *)
(*                                                                          *)
(* Before, the flag was the thread's (or, for the pump on a worker, a     *)
(* static) and only a pump that ended on a read error lowered it: a pump  *)
(* dropped with its runtime left the flag raised, and a listener polled   *)
(* there started no pump and was never woken.                              *)
(*                                                                          *)
(* What is abstract: the handler's and the dispatch's steps are those of  *)
(* Signal.tla, which checks their order with pumps that never go; here    *)
(* one place's flag, the pumps it starts (each one task, numbered in the  *)
(* order they start), and `Drops`, how many of them may be dropped.       *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    N,         \* deliveries of the signal
    Drops,     \* how many pumps may be dropped, in all
    Mutation   \* "none", or the name of the one rule this run takes away

\* Every pump that can start: one more than may be dropped.
Pumps == 1..(Drops + 1)
\* The handler's and the listener's numbers: no pump has them.
HandlerT == 0
ListenerT == -1

(* --algorithm SignalPump
variables
    count = 0,        \* deliveries counted
    wake = 0,         \* writes to the wake descriptor not drained
    dispatched = 0,   \* the count the last dispatch woke for
    waker = FALSE,    \* the listener's waker is registered
    woken = FALSE,    \* a wake came for the sleeping listener
    seen = 0,         \* the count the listener last saw
    items = 0,        \* (record) items the listener took
    flag = FALSE,     \* the place's flag: a pump serves listeners polled here
    alive = {},       \* the pumps whose tasks live
    started = 0,      \* pumps started so far
    dropped = 0;      \* pumps dropped so far

\* The handler: each delivery is counted, then written to the descriptor.
fair process handler = HandlerT
begin
H_loop:
    while count < N do
        count := count + 1;
H_write:
        wake := wake + 1;
    end while;
end process;

\* A pump: started by a listener, it waits, drains, dispatches, until it is dropped.
fair process pump \in Pumps
variables now = 0, moved = FALSE;
begin
P_start:
    \* It runs once a listener started it.
    await self \in alive;
P_loop:
    while TRUE do
        either
            \* The descriptor became readable: drain it ...
            await wake > 0;
            wake := 0;
P_load:
            \* ... look at the count ...
            now := count;
P_swap:
            \* ... swap it into the one the last dispatch woke for ...
            moved := dispatched # now;
            dispatched := now;
P_wake:
            \* ... and wake the listener if it moved.
            if moved /\ waker then
                waker := FALSE;
                woken := TRUE;
            end if;
        or
            \* Its task is dropped between two polls (its shard went), while drops are
            \* left: the pump never runs again.
            await dropped < Drops;
            dropped := dropped + 1;
            alive := alive \ {self};
P_down:
            \* The drop lowers the place's flag first ...
            \* MUTATION pump_dropped: the drop leaves the flag raised (the flag was the
            \* thread's, which the task's drop cannot reach).
            \* MUTATION wake_before_down: the listeners are woken before the flag is lowered.
            if Mutation = "wake_before_down" then
                if waker then waker := FALSE; woken := TRUE; end if;
            elsif Mutation # "pump_dropped" then
                flag := FALSE;
            end if;
P_wake_all:
            \* ... then wakes every listener, so each polls again.
            \* MUTATION no_wake_at_drop: the drop wakes nobody.
            if Mutation = "wake_before_down" then
                flag := FALSE;
            elsif Mutation # "no_wake_at_drop" /\ Mutation # "pump_dropped" /\ waker then
                waker := FALSE;
                woken := TRUE;
            end if;
            goto Done;
        end either;
    end while;
end process;

\* The listener: takes an item when the count moved, else waits for it.
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
L_ensure:
            \* `ensure_pump`: the listener that raises the flag starts a pump.
            if ~flag then
                flag := TRUE;
                alive := alive \cup {started + 1};
                started := started + 1;
            end if;
L_recheck:
            \* One more look before it sleeps.
            if count # seen then
                seen := count;
                items := items + 1;
            else
L_sleep:
                \* A task sleeps until its waker is woken.
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

\* The listener sees every delivery, however many pumps were dropped.
SeesAll == <>(seen = N)

\* It never takes more items than there were deliveries.
NoExtraItem == items <= N

\* One pump at most serves the place at a time, and no more start than can.
OnePump == Cardinality(alive) <= 1 /\ started <= Drops + 1

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A pump starts again after one was dropped.
NoRestart == started < 2
=============================================================================

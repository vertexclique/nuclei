----------------------------- MODULE AsyncFd -----------------------------
(***************************************************************************)
(* A caller's own descriptor driven by readiness (src/io/async_fd.rs and   *)
(* the side's state for `Kind::Fd`), at the grain of each atomic access;   *)
(* a caller's clear is `Source::fd_clear_read` in                          *)
(* src/io/source/read/calls.rs (`Side::run_or_defer` in src/io/side.rs,    *)
(* `LeftClear` in src/io/source/clear.rs, applied by `read_rare` in        *)
(* src/io/source/hooks.rs), and the write side's `fd_clear_write` is the   *)
(* same code.                                                              *)
(*                                                                          *)
(* In plain words: a peer writes; the driver tells the side the socket is  *)
(* ready. On epoll and kqueue (edge-triggered) each write brings an edge,  *)
(* which marks the side ready and counts it. On io_uring a poll is armed   *)
(* while the side is not ready and a task waits, and completes once the   *)
(* socket holds bytes (it looks at the socket as it is: level), marking    *)
(* the side ready and counting it. A task waits until the side is ready,   *)
(* notes the count (its guard's tick), and makes its own call: bytes, it  *)
(* takes some and goes on; none (`WouldBlock`), it clears the side's ready *)
(* only if the count is still its tick, since an edge after its call is   *)
(* the only one it will get. It must take every byte.                     *)
(*                                                                          *)
(* With more than one task, a task's look holds the side's guard for a    *)
(* moment, and an edge that comes meanwhile is served by that holder as it *)
(* lets go. A clear that finds the side held leaves its tick for the       *)
(* holder, who clears the side as it lets go, before anyone can look      *)
(* again; of several ticks left at once the newest is kept, since only it *)
(* can still clear. Before the fix the clear went through a poll of the   *)
(* side, which found it held and was dropped: the task then found the     *)
(* side ready again with no edge since its clear, and called again, and    *)
(* again, for as long as the other tasks kept the side busy.              *)
(*                                                                          *)
(* What must never happen: a byte that is never taken; a task that finds  *)
(* the side ready though no edge came since its own clear                 *)
(* (`NoSpuriousReady`).                                                   *)
(*                                                                          *)
(* What is abstract: a task's wait is `await` on the side's ready (the    *)
(* wake is the side's, checked in `side`); a look that finds the side held *)
(* waits until it is free (its waker is woken by the holder's release); a *)
(* call takes one byte or more; a clear that finds the side free takes,   *)
(* clears and lets go in one step.                                        *)
(***************************************************************************)
EXTENDS Integers, TLC                                                        \* numbers and TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Writes,    \* bytes the peer writes, one per write
    Ring,      \* TRUE: io_uring's armed poll; FALSE: epoll's and kqueue's edges
    Tasks,     \* how many tasks look at the side and make their own calls
    Mutation   \* "none", or the name of the one rule this run takes away

TaskIds == 1..Tasks                                                          \* the tasks, by number
Max(a, b) == IF a > b THEN a ELSE b                                          \* the larger of two numbers

(* --algorithm AsyncFd
variables
    data = 0,          \* bytes in the socket
    taken = 0,         \* bytes the tasks took
    written = 0,       \* bytes the peer wrote
    ready = TRUE,      \* the side says the socket may be ready (it starts so)
    edges = 0,         \* the side's count of what made it ready
    waiting = {},      \* the tasks that wait for the side to be ready
    kept = FALSE,      \* (record) a clear found a newer edge and kept the ready
    holder = 0,        \* the task whose look holds the side's guard now; 0: nobody
    edgeLeft = FALSE,  \* an edge came while the side was held: its holder serves it as it lets go
    pend = -1,         \* the tick a clear left for the holder (`LeftClear`); -1: none
    cleared = [t \in TaskIds |-> -1], \* the tick of each task's last clear; -1: none yet
    spurious = FALSE;  \* (record) a task found the side ready with no edge since its own clear

\* An edge (epoll's, kqueue's, or a ring's poll completing): served now if the side is free,
\* else left for its holder, who serves it as it lets go.
macro edge() begin
    if holder = 0 then                                                       \* the side is free:
        ready := TRUE;                                                       \* it may be ready
        edges := edges + 1;                                                  \* and counts the edge
    else                                                                     \* held:
        edgeLeft := TRUE;                                                    \* the holder serves it
    end if;
end macro;

\* The clear of tick `t` applied to the side: only if no edge came since the look it is of.
macro apply(t) begin
    \* MUTATION no_tick: the clear does not look at the count.
    if edges = t \/ Mutation = "no_tick" then                                \* no edge since the look:
        ready := FALSE;                                                      \* the side waits for the next
    else                                                                     \* an edge came since:
        kept := TRUE;                                                        \* the side stays ready
    end if;
end macro;

fair process peer = 100                                                     \* numbered past the tasks
begin
W_loop:
    while written < Writes do
W_write:
        data := data + 1;                                                    \* one byte in the socket
        written := written + 1;                                              \* counted
W_edge:
        if ~Ring then                                                        \* epoll and kqueue report it
            edge();                                                          \* with an edge
        end if;
    end while;
end process;

\* io_uring: a poll armed while the side is not ready and a task waits; it
\* completes once the socket holds bytes.
fair process poller = 101                                                   \* likewise
begin
P_loop:
    while Ring do
P_arm:
        await ~ready /\ waiting # {};                                        \* armed for a task that waits
P_complete:
        await data > 0;                                                      \* the socket holds bytes:
        edge();                                                              \* its completion is an edge
    end while;
end process;

fair process task \in TaskIds
variables
    tick = 0,      \* the count the task's last look saw
    seen = FALSE;  \* its last look found the side ready
begin
T_loop:
    while taken < Writes do
T_take:
        \* The task's look takes the side's guard; one that finds it held is woken as it lets go.
        await holder = 0;
        holder := self;
T_look:
        if ready then                                                        \* the side may be ready:
            if edges = cleared[self] then                                    \* though no edge came since its clear
                spurious := TRUE;                                            \* (record) a ready that is not one
            end if;
            tick := edges;                                                   \* the guard's tick
            seen := TRUE;                                                    \* the task makes its call
        else                                                                 \* not ready:
            waiting := waiting \cup {self};                                  \* the task waits for it
            seen := FALSE;
        end if;
T_release:
        \* The look lets go, doing first what others left meanwhile, one thing a step: a clear's
        \* tick, then an edge; it lets go only once nothing is left (`release_word`).
        while holder = self do
            if pend # -1 then                                                \* a clear was left:
                apply(pend);                                                 \* applied by the holder
                pend := -1;                                                  \* and taken
            elsif edgeLeft then                                              \* an edge was left:
                ready := TRUE;                                               \* served by the holder
                edges := edges + 1;
                edgeLeft := FALSE;
            else                                                             \* nothing left:
                holder := 0;                                                 \* the side is free
            end if;
        end while;
T_next:
        if ~seen then                                                        \* the task waits:
T_wait:
            await ready;                                                     \* until the side is ready
            waiting := waiting \ {self};                                     \* and looks again
        else
T_call:
            if data > 0 then                                                 \* bytes in the socket:
                with n \in 1..data do                                        \* the call takes some
                    data := data - n;
                    taken := taken + n;
                end with;
            else                                                             \* `WouldBlock`:
T_clear:
                cleared[self] := tick;                                       \* the task clears its look
                if holder = 0 then                                           \* the side is free:
                    apply(tick);                                             \* cleared now
                \* MUTATION drop_held_clear: the clear is a poll that finds the side held and is lost.
                elsif Mutation = "drop_held_clear" then
                    skip;
                \* MUTATION clear_last_wins: the tick left replaces any left before, though older.
                elsif Mutation = "clear_last_wins" then
                    pend := tick;
                else                                                         \* held: the tick is left
                    pend := Max(pend, tick);                                 \* the newest of those left
                end if;
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

\* The task takes no more than came.
Counted == taken + data = written

\* Every byte written is taken.
AllTaken == <>(taken = Writes)

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* No task finds the side ready though no edge came since its own clear: it would call again,
\* find nothing, and spin.
NoSpuriousReady == ~spurious

\* A clear finds an edge newer than its tick and keeps the side ready.
NoKept == ~kept
=============================================================================

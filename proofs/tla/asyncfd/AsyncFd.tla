----------------------------- MODULE AsyncFd -----------------------------
(***************************************************************************)
(* A caller's own descriptor driven by readiness (src/io/async_fd.rs and   *)
(* the side's state for `Kind::Fd`), at the grain of each atomic access.   *)
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
(* What is abstract: a task's wait is `await` on the side's ready (the    *)
(* wake is the side's, checked in `side`); a call takes one byte or more. *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Writes,    \* bytes the peer writes, one per write
    Ring,      \* TRUE: io_uring's armed poll; FALSE: epoll's and kqueue's edges
    Mutation   \* "none", or the name of the one rule this run takes away

(* --algorithm AsyncFd
variables
    data = 0,          \* bytes in the socket
    taken = 0,         \* bytes the task took
    written = 0,
    ready = TRUE,      \* the side says the socket may be ready (it starts so)
    edges = 0,         \* the side's count of what made it ready
    waiting = FALSE,   \* the task waits for the side
    kept = FALSE;      \* (record) a clear found a newer edge and kept the ready

fair process peer = "peer"
begin
W_loop:
    while written < Writes do
W_write:
        data := data + 1;
        written := written + 1;
W_edge:
        if ~Ring then
            ready := TRUE;
            edges := edges + 1;
        end if;
    end while;
end process;

\* io_uring: a poll armed while the side is not ready and a task waits; it
\* completes once the socket holds bytes.
fair process poller = "poller"
begin
P_loop:
    while Ring do
P_arm:
        await ~ready /\ waiting;
P_complete:
        await data > 0;
        ready := TRUE;
        edges := edges + 1;
    end while;
end process;

fair process task = "task"
variables tick = 0;
begin
T_loop:
    while taken < Writes do
T_wait:
        waiting := TRUE;
T_ready:
        await ready;
        waiting := FALSE;
        tick := edges;
T_call:
        if data > 0 then
            with n \in 1..data do
                data := data - n;
                taken := taken + n;
            end with;
        else
T_clear:
            \* MUTATION no_tick: the clear does not look at the count.
            if edges = tick \/ Mutation = "no_tick" then
                ready := FALSE;
            else
                kept := TRUE;
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

\* A clear finds an edge newer than its tick and keeps the side ready.
NoKept == ~kept
=============================================================================

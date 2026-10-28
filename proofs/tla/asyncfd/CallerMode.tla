---------------------------- MODULE CallerMode ----------------------------
(***************************************************************************)
(* A socket's read side turning from the handle's own reads to its        *)
(* caller's calls (`try_io`, `async_io`; src/io/source/read.rs `fd_ready`, *)
(* read/ops.rs), at the grain of each atomic access.                      *)
(*                                                                          *)
(* In plain words: the task reads once through the handle, which reads    *)
(* ahead (all the socket holds) and, on io_uring, keeps a multishot       *)
(* receive out that brings what comes later into the read-ahead. Then the *)
(* task makes calls of its own on the socket. A call while the handle      *)
(* holds data read ahead fails (busy), and the task reads that data        *)
(* through the handle first; a receive still out is stopped, the task      *)
(* waits for it to end and looks again (it may have brought bytes). After *)
(* that the side is the caller's: the driver says when it may be ready    *)
(* (an edge, or a poll armed on a ring), and a call that finds nothing     *)
(* clears the readiness only if no edge came since its look. Every byte   *)
(* must be taken once, in the order it was written.                        *)
(*                                                                          *)
(* What is abstract: the task's waits are on `woken`, set by whatever     *)
(* wakes it in the code (an edge, a poll's end, the wake after a stopped   *)
(* receive ends with the side ready); a call takes every byte there is.   *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    Writes,    \* bytes the peer writes, one per write
    Ring,      \* TRUE: io_uring; FALSE: epoll and kqueue
    Mutation   \* "none", or the name of the one rule this run takes away

(* --algorithm CallerMode
variables
    sock = <<>>,        \* bytes in the socket, oldest first
    held = <<>>,        \* bytes the handle read ahead
    taken = <<>>,       \* bytes the task took, in the order it took them
    written = 0,
    caller = FALSE,     \* the side takes its caller's calls
    ready = TRUE,       \* the side may be ready (caller's calls)
    edges = 0,
    recvOut = FALSE,    \* ring: the handle's multishot receive is out
    stopping = FALSE,   \* it is being stopped
    pollOut = FALSE,    \* ring: the caller's poll is out
    waiting = FALSE,    \* the task waits on the side
    woken = FALSE,
    busy = FALSE,       \* (record) a call found data read ahead
    stoppedEmpty = FALSE; \* (record) a stop ended the receive with nothing

define
    \* What ends an operation: a waiting caller is woken if the side may be
    \* ready, else its poll goes out (`submit_ops` after the end).
    AfterEnd(w, r) == caller /\ w /\ r
end define;

fair process peer = "peer"
begin
W_loop:
    while written < Writes do
W_write:
        sock := Append(sock, written + 1);
        written := written + 1;
        if ~Ring then
            \* An edge: the side may be ready; a waiting caller is woken.
            ready := TRUE;
            edges := edges + 1;
            if caller /\ waiting then
                woken := TRUE;
            end if;
        end if;
    end while;
end process;

\* io_uring's completions.
fair process ring = "ring"
begin
R_loop:
    while Ring do
        either
            \* The multishot receive brings what came; stopped, it ends.
            await recvOut /\ sock # <<>>;
            held := held \o sock;
            sock := <<>>;
            if stopping then
                recvOut := FALSE;
                stopping := FALSE;
                if AfterEnd(waiting, ready) /\ Mutation # "no_wake" then
                    woken := TRUE;
                elsif caller /\ waiting then
                    pollOut := TRUE;
                end if;
            end if;
        or
            \* The stop ends it with nothing brought.
            await recvOut /\ stopping;
            recvOut := FALSE;
            stopping := FALSE;
            stoppedEmpty := TRUE;
            \* MUTATION no_wake: the side is ready, and nobody wakes the caller.
            if AfterEnd(waiting, ready) /\ Mutation # "no_wake" then
                woken := TRUE;
            elsif caller /\ waiting then
                pollOut := TRUE;
            end if;
        or
            \* The caller's poll ends once the socket holds bytes.
            await pollOut /\ sock # <<>>;
            pollOut := FALSE;
            ready := TRUE;
            edges := edges + 1;
            woken := TRUE;
        end either;
    end while;
end process;

fair process task = "task"
variables tick = 0;
begin
T_first:
    \* One read of the handle's own: the driver reads ahead all there is,
    \* the read takes one byte, and on a ring the receive stays out.
    await sock # <<>>;
    with all = held \o sock do
        taken := Append(taken, Head(all));
        held := Tail(all);
    end with;
    sock := <<>>;
    if Ring then
        recvOut := TRUE;
    end if;
T_loop:
    while Len(taken) < Writes do
T_enter:
        \* MUTATION no_held_check: the call runs with data read ahead.
        if held # <<>> /\ Mutation # "no_held_check" then
            busy := TRUE;
            caller := FALSE;
T_drain:
            \* The handle's own reads take it, and the ring's receive goes on.
            taken := taken \o held;
            held := <<>>;
            if Ring then
                recvOut := TRUE;
            end if;
        else
            caller := TRUE;
T_stop:
            if recvOut then
                stopping := TRUE;
                waiting := TRUE;
                woken := FALSE;
T_wait_stop:
                await woken;
                waiting := FALSE;
            else
T_look:
                if ready then
                    tick := edges;
T_call:
                    if sock # <<>> then
                        taken := taken \o sock;
                        sock := <<>>;
                    elsif edges = tick then
                        ready := FALSE;
                    end if;
                else
                    waiting := TRUE;
                    woken := FALSE;
                    if Ring then
                        pollOut := TRUE;
                    end if;
T_wait:
                    await woken;
                    waiting := FALSE;
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

\* Bytes are taken once, in the order they were written.
InOrder == \A i \in 1..Len(taken) : taken[i] = i

\* Every byte written is taken.
AllTaken == <>(Len(taken) = Writes)

(***************************************************************************)
(* Witnesses.                                                              *)
(***************************************************************************)

NoBusy == ~busy
NoStoppedEmpty == ~stoppedEmpty
=============================================================================

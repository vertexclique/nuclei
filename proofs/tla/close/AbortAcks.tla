------------------------------ MODULE AbortAcks ------------------------------
(***************************************************************************)
(* A TCP stream a task set to zero linger and dropped, with bytes still  *)
(* in its write-behind, or with none behind but bytes the kernel holds    *)
(* unacknowledged (src/io/handle.rs: the handle's drop and `linger`;      *)
(* src/io/source.rs: `Source::waits_for_acks`; src/io/sys: `unacked`, the *)
(* kernel's count of the bytes it holds that the peer has not             *)
(* acknowledged).                                                          *)
(*                                                                          *)
(* Who acts. The handle drops: with bytes behind, or bytes in the kernel   *)
(* not acknowledged yet, a linger task takes the socket over; with neither *)
(* it closes at once. The linger task drains the write-behind into the     *)
(* kernel's send queue. The writer's kernel sends what it holds onto the   *)
(* wire, one byte a segment, in order. The reader's kernel takes each      *)
(* segment and acknowledges it. Then the socket closes with no linger: the *)
(* kernel drops what it has not sent and sends a reset. On a loopback the  *)
(* reset may overtake the data still on the wire (macOS's does): the       *)
(* reader then takes the reset for a stale one, answers with a challenge   *)
(* ACK, and the writer's closed port answers nothing (the runner drops     *)
(* resets for closed ports), so the reset is lost and the reader waits for *)
(* good. A clock may end the linger's time at any moment.                  *)
(*                                                                          *)
(* The fix. A handle dropped with nothing behind but bytes the kernel      *)
(* holds unacknowledged goes to the linger task too. After the drain the   *)
(* linger waits until the kernel holds no byte the peer has not            *)
(* acknowledged, or the linger's time ran out, and only then closes:       *)
(* nothing is on the wire for the reset to overtake.                       *)
(*                                                                          *)
(* What must never happen. The reader neither takes the reset nor saw the  *)
(* linger's time run out: a stream left open for good. And a reset that    *)
(* came with no time run out must come after every byte the task wrote.    *)
(*                                                                          *)
(* What is abstract. Bytes are counted; a segment is one byte; the         *)
(* kernel's queue holds `Room`; the reader always has room; the linger's   *)
(* time running out is a step that may come at any moment, or never.      *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC                                            \* numbers, sequences, TLC's helpers

CONSTANTS                                                                   \* what each configuration sets:
    Bytes,      \* bytes in the write-behind as the handle drops
    Kernel,     \* bytes the task's writes handed the kernel before the drop, not sent yet (at most Room)
    Room,       \* bytes the writer kernel's send queue holds, sent or not
    Mutation    \* "none"; "close_at_drain": the close comes as the drain ends, as before the fix;
                \* "close_at_once": a drop with nothing behind closes at once, as before this change

(* --algorithm AbortAcks                                                      \* the algorithm, in PlusCal
variables                                                                   \* the shared state:
    behind = Bytes,       \* bytes still in the write-behind
    queued = Kernel,      \* bytes in the writer kernel's queue, not sent yet
    unacked = 0,          \* bytes sent and not acknowledged yet
    sent = 0,             \* bytes the writer's kernel sent
    wire = <<>>,          \* the data segments in flight to the reader, in order
    rst = FALSE,          \* the reset is in flight; it may overtake the data on the wire
    closed = FALSE,       \* the writer's socket is closed: its port answers nothing
    got = 0,              \* bytes the reader took
    reset = FALSE,        \* the reader took the reset
    lost = FALSE,         \* the reader dropped the reset, which overtook data
    expired = FALSE;      \* the linger's time ran out

\* The handle's drop, then the linger task: drains the write-behind, waits for the acknowledgements, and closes.
fair process linger = "linger"                                              \* weakly fair, like the other actors
begin                                                                       \* the linger's steps:
L_drop:                                                                     \* the handle drops, the kernel's count read once
    \* MUTATION close_at_once: a drop with nothing behind closes at once, whatever the kernel still holds.
    if behind = 0 /\ (Mutation = "close_at_once" \/ queued + unacked = 0) then \* nothing behind, and nothing unacknowledged:
        goto L_close;                                                       \* the socket closes at once, no task takes it
    end if;                                                                 \* else the linger task takes the socket over
L_drain:                                                                    \* the drain
    while behind > 0 /\ ~expired do                                         \* while bytes are behind and time is left
        await queued + unacked < Room \/ expired;                           \* room in the kernel, or the time ran out
        if ~expired then                                                    \* time is left:
            behind := behind - 1;                                           \* a byte leaves the write-behind
            queued := queued + 1;                                           \* into the kernel's queue
        end if;                                                             \* the byte is handed over
    end while;                                                              \* the drain is over
L_wait:                                                                     \* the wait for the acknowledgements
    \* MUTATION close_at_drain: no wait: the close comes as soon as the drain is over.
    if Mutation # "close_at_drain" then                                     \* the fix:
        await (queued = 0 /\ unacked = 0) \/ expired;                       \* nothing unacknowledged, or the time ran out
    end if;                                                                 \* the wait is over
L_close:                                                                    \* the close with no linger
    closed := TRUE;                                                         \* the socket goes
    queued := 0;                                                            \* the kernel drops what it has not sent
    rst := TRUE;                                                            \* and sends the reset
end process;                                                                \* end of the linger

\* The writer's kernel: sends what its queue holds, until the socket closes.
fair process sender = "sender"                                              \* weakly fair
begin                                                                       \* the sender's steps:
S_loop:                                                                     \* one segment
    while ~closed do                                                        \* while the socket is open
        await queued > 0 \/ closed;                                         \* a byte to send, or the close
        if ~closed then                                                     \* still open:
            queued := queued - 1;                                           \* the byte leaves the queue
            unacked := unacked + 1;                                         \* waits for its acknowledgement
            sent := sent + 1;                                               \* counted
            wire := Append(wire, sent + 1);                                 \* and goes onto the wire
        end if;                                                             \* the segment is out
    end while;                                                              \* the socket closed
end process;                                                                \* end of the sender

\* The reader's kernel: takes the data in order, and the reset if it comes after it.
fair process reader = "reader"                                              \* weakly fair
begin                                                                       \* the reader's steps:
R_loop:                                                                     \* one arrival
    while ~reset /\ ~(lost /\ wire = <<>>) do                               \* until the reset, or a lost one with nothing left
        either                                                              \* a data segment, in order:
            await wire # <<>>;                                              \* one in flight
            wire := Tail(wire);                                             \* taken off the wire
            got := got + 1;                                                 \* read
            if ~closed then                                                 \* the writer still open:
                unacked := unacked - 1;                                     \* the acknowledgement reaches it
            end if;                                                         \* (a closed port answers nothing)
        or                                                                  \* the reset, which may overtake the data:
            await rst;                                                      \* in flight
            rst := FALSE;                                                   \* it arrives
            if wire = <<>> then                                             \* nothing before it on the wire:
                reset := TRUE;                                              \* its sequence is the next byte: taken
            else                                                            \* data still in flight before it:
                lost := TRUE;                                               \* a challenge ACK the closed port never answers
            end if;                                                         \* the reset is handled
        end either;                                                         \* the arrival is over
    end while;                                                              \* the reader is done
end process;                                                                \* end of the reader

\* The linger's time may run out at any moment, or never: not fair.
process clock = "clock"                                                     \* may never act
begin                                                                       \* its one step:
C_tick:                                                                     \* the deadline
    expired := TRUE;                                                        \* the linger's time ran out
end process;                                                                \* end of the clock
end algorithm; *)                                                           \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* The reader takes the reset, unless the linger's time ran out: never a stream left open for good.
ResetOrDeadline == <>(reset \/ expired)                                     \* sometime, one or the other

\* A reset that came with time left comes after every byte the task wrote.
WholeBeforeReset == (reset /\ ~expired) => got = Bytes + Kernel             \* every byte, then the reset

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* The linger's time runs out with data on the wire, and the reset is lost: the one way left.
NoDeadlineLoss == ~(expired /\ lost)                                        \* never both
=============================================================================

------------------------------ MODULE AbortAcks ------------------------------
(***************************************************************************)
(* A TCP stream a task set to zero linger and dropped, with bytes still  *)
(* in its write-behind, or with none behind but bytes the kernel holds    *)
(* unacknowledged (src/io/handle.rs: the handle's drop, `linger` and      *)
(* `wait_for_acks`; src/io/source.rs: `Source::waits_for_acks`; src/io/sys:*)
(* `unacked`, the kernel's count of the bytes it holds that the peer has  *)
(* not acknowledged).                                                     *)
(*                                                                          *)
(* Who acts. The handle drops: with bytes behind a linger task takes the  *)
(* socket over and drains nuclei's write-behind into the kernel's send    *)
(* queue; with nothing behind it closes at once. The writer's kernel sends *)
(* what it holds onto the wire, one byte a segment, in order. The reader's *)
(* kernel takes each segment and, when it acknowledges (`Acks`), its       *)
(* acknowledgement reaches the writer. Then the socket closes with no      *)
(* linger: the kernel drops what it has not sent and sends a reset, as a   *)
(* zero linger asks. A clock may end the drain's time (the runtime's       *)
(* `write_behind_linger`) at any moment, or never.                         *)
(*                                                                          *)
(* macOS's loopback (`Platform = "mac_loopback"`) may let the reset        *)
(* overtake the data still on the wire: the reader then takes the reset    *)
(* for a stale one, answers with a challenge ACK, and the writer's closed  *)
(* port answers nothing, so the reset is lost and the reader waits for     *)
(* good. There, and only there, the close waits until the kernel holds no  *)
(* byte the peer has not acknowledged, for at most a short cap of its own  *)
(* (`ACK_CAP`, a few milliseconds, not the drain's time), a drop with      *)
(* nothing behind but bytes unacknowledged going to the linger task for    *)
(* it. Everywhere else (`"other"`) the wire keeps its order, the reset     *)
(* comes after the data, and the close comes at once after the drain.      *)
(*                                                                          *)
(* What must never happen. The socket never closes (`Closes`): a peer that *)
(* never acknowledges must not hold the close for the drain's time, and    *)
(* an uncapped wait would (`uncapped`). The close comes before nuclei's    *)
(* write-behind went into the kernel, with time left (`DrainedBeforeClose`).*)
(* The reader neither takes the reset nor saw a bound run out: a stream   *)
(* left open for good (`ResetOrBound`).                                    *)
(*                                                                          *)
(* What is abstract. Bytes are counted; a segment is one byte; the         *)
(* kernel's queue holds `Room`; the reader always has room; the drain's    *)
(* time running out is a step that may come at any moment, or never; the  *)
(* cap runs out at some moment once the wait began, and always does. That *)
(* the loopback acknowledges within the cap is the bet the cap makes, not  *)
(* a fact the model can check: a cap that runs out with data on the wire   *)
(* may still lose the reset (`W_cap_loss`).                                *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC                                            \* numbers, sequences, TLC's helpers

CONSTANTS                                                                   \* what each configuration sets:
    Bytes,      \* bytes in the write-behind as the handle drops
    Kernel,     \* bytes the task's writes handed the kernel before the drop, not sent yet (at most Room)
    Room,       \* bytes the writer kernel's send queue holds, sent or not
    Platform,   \* "mac_loopback": a stream to a loopback address on macOS, whose wire may let the reset
                \* overtake the data; "other": any other, whose wire keeps its order
    Acks,       \* TRUE: the reader's kernel acknowledges what it takes; FALSE: a peer that never does
    Mutation    \* "none"; "close_at_drain": no wait after the drain on macOS's loopback;
                \* "close_at_once": a drop with nothing behind closes at once there too;
                \* "uncapped": the wait is bounded only by the drain's time

\* Whether the reset may overtake the data on the wire, and so whether the close waits for the acknowledgements.
Reorders == Platform = "mac_loopback"

(* --algorithm AbortAcks                                                      \* the algorithm, in PlusCal
variables                                                                   \* the shared state:
    behind = Bytes,       \* bytes still in the write-behind
    queued = Kernel,      \* bytes in the writer kernel's queue, not sent yet
    unacked = 0,          \* bytes sent and not acknowledged yet
    sent = 0,             \* bytes the writer's kernel sent
    wire = <<>>,          \* the data segments in flight to the reader, in order
    rst = FALSE,          \* the reset is in flight
    closed = FALSE,       \* the writer's socket is closed: its port answers nothing
    got = 0,              \* bytes the reader took
    reset = FALSE,        \* the reader took the reset
    lost = FALSE,         \* the reader dropped the reset, which overtook data
    expired = FALSE,      \* the drain's time ran out
    waiting = FALSE,      \* the linger waits for the acknowledgements
    capped = FALSE;       \* the wait's own cap ran out

\* The handle's drop, then the linger task: drains the write-behind, waits where it must, and closes.
fair process linger = "linger"                                              \* weakly fair, like the other actors
begin                                                                       \* the linger's steps:
L_drop:                                                                     \* the handle drops, the kernel's count read once
    \* MUTATION close_at_once: a drop with nothing behind closes at once on macOS's loopback too.
    if behind = 0 /\ ~(Reorders /\ Mutation # "close_at_once" /\ queued + unacked > 0) then
        goto L_close;                                                       \* nothing to drain or wait for: it closes at once
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
    \* MUTATION close_at_drain: no wait on macOS's loopback either.
    if Reorders /\ Mutation # "close_at_drain" then                         \* only where the reset may overtake:
        waiting := TRUE;                                                    \* the wait begins, and its cap with it
L_waiting:                                                                  \* until the bytes are acknowledged
        \* MUTATION uncapped: the wait is bounded only by the drain's time.
        await (queued = 0 /\ unacked = 0) \/ (IF Mutation = "uncapped" THEN expired ELSE capped);
        waiting := FALSE;                                                   \* the wait is over
    end if;                                                                 \* elsewhere there is none
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

\* The reader's kernel: takes the data in order, and the reset.
fair process reader = "reader"                                              \* weakly fair
begin                                                                       \* the reader's steps:
R_loop:                                                                     \* one arrival
    while ~reset /\ ~(lost /\ wire = <<>>) do                               \* until the reset, or a lost one with nothing left
        either                                                              \* a data segment, in order:
            await wire # <<>>;                                              \* one in flight
            wire := Tail(wire);                                             \* taken off the wire
            got := got + 1;                                                 \* read
            if ~closed /\ Acks then                                         \* the writer still open, and the reader acks:
                unacked := unacked - 1;                                     \* the acknowledgement reaches it
            end if;                                                         \* (a closed port answers nothing)
        or                                                                  \* the reset: on macOS's loopback it may overtake
            await rst /\ (Reorders \/ wire = <<>>);                         \* the data; elsewhere it comes after it
            rst := FALSE;                                                   \* it arrives
            if wire = <<>> then                                             \* nothing before it on the wire:
                reset := TRUE;                                              \* its sequence is the next byte: taken
            else                                                            \* data still in flight before it:
                lost := TRUE;                                               \* a challenge ACK the closed port never answers
            end if;                                                         \* the reset is handled
        end either;                                                         \* the arrival is over
    end while;                                                              \* the reader is done
end process;                                                                \* end of the reader

\* The drain's time may run out at any moment, or never: not fair.
process clock = "clock"                                                     \* may never act
begin                                                                       \* its one step:
C_tick:                                                                     \* the deadline
    expired := TRUE;                                                        \* the drain's time ran out
end process;                                                                \* end of the clock

\* The wait's own cap: once the wait began it runs out at some moment, and always does.
fair process cap = "cap"                                                    \* weakly fair: a fixed short bound
begin                                                                       \* its one step:
K_tick:                                                                     \* the cap
    await waiting;                                                          \* once the wait began
    capped := TRUE;                                                         \* the cap ran out
end process;                                                                \* end of the cap
end algorithm; *)                                                           \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* The socket closes in the end: no peer holds the close for good.
Closes == <>closed                                                          \* sometime

\* The close comes only once nuclei's write-behind went into the kernel, unless the drain's time ran out.
DrainedBeforeClose == (closed /\ ~expired) => behind = 0                    \* the write-behind first

\* The reader takes the reset, unless a bound ran out (the drain's time, or the wait's cap).
ResetOrBound == <>(reset \/ expired \/ capped)                              \* sometime, one or the other

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* The cap runs out with data on the wire, and the reset is lost: what the cap bets against.
NoCapLoss == ~(capped /\ lost)                                              \* never both
=============================================================================

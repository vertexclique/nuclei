----------------------------- MODULE Oneshot -----------------------------
(***************************************************************************)
(* The one-shot channel of nuclei::sync::oneshot (src/sync/oneshot.rs on   *)
(* the waker cells of src/sync/atomic_waker.rs), at the grain of its       *)
(* atomic steps.                                                           *)
(*                                                                          *)
(* In plain words: a channel carries one value from one sender to one       *)
(* receiver. The sender writes the value into a cell and then raises two    *)
(* bits of one state word in a single step (VALUE_SENT and TX_DROPPED);     *)
(* if the receiver had closed first, it takes its value back and the send   *)
(* fails. The receiver looks at the word, and while there is nothing to     *)
(* see it registers its waker and looks again before it sleeps; a wake      *)
(* makes it look once more. Closing (or dropping) the receiver raises       *)
(* RX_CLOSED and wakes a sender that waits in closed(). Whoever owns the    *)
(* cell when the dust settles reads the value out exactly once: the         *)
(* receiver by receiving, the receiver's drop for a value nobody took, or   *)
(* the sender of a failed send.                                            *)
(*                                                                          *)
(* What is modelled step by step: the three bits of the state word, the     *)
(* value cell, both handles, the receiver's own close note (open, with a    *)
(* value, empty), every wake of either task, and both register-then-look-   *)
(* again handshakes: the receiver's poll and the sender's closed() poll.    *)
(*                                                                          *)
(* What is abstract: each waker cell is one flag that a register fills      *)
(* and a wake empties (the cell's own two-phase handshake is checked by     *)
(* the loom models in sync::loom_tests::signals); the receiver's close and  *)
(* drop do their fetch-or and the wake it may trigger in one step here      *)
(* (an interleaving between the two changes nothing either side answers    *)
(* for; the sender's publish and its wake stay two steps, because the       *)
(* receiver's drop can land between them); the destructor of the value     *)
(* runs inside the step that reads the cell; and the value itself is one   *)
(* unlabelled token, so "which value" is beyond the model (there is only   *)
(* ever one). A send after the sender is spent cannot be written: the      *)
(* Rust method consumes the sender, and so does the model, so "a second    *)
(* send fails" is checked in the form the API allows: a send that races a  *)
(* closed receiver fails and the value comes back.                         *)
(*                                                                          *)
(* `Mutation` names one rule taken away, so that a configuration can show  *)
(* the property that rule protects really is checked (see README).         *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    Prog,      \* what the sender does, in order: a sequence of "send", "park", "drop"
    RxProg,    \* what the receiver does, in order: "recv", "try", "close", "drop"
    Mutation   \* "none", or the name of the one rule this run takes away

\* The sender thread's number.
Tx == 1
\* The receiver thread's number.
Rx == 0

(* --algorithm Oneshot
variables
    \* ---- the state word (one atomic word in the code) ----
    valueSent = FALSE,       \* VALUE_SENT: the sender wrote the value and published it
    rxClosed = FALSE,        \* RX_CLOSED: the receiver closed or was dropped
    txDropped = FALSE,       \* TX_DROPPED: the sender is gone: it sent, or was dropped
    \* ---- the value cell ----
    valueInCell = FALSE,     \* the cell holds the value, written and not yet read
    \* ---- the waker cells (one flag each; see the header) ----
    rxRegistered = FALSE,    \* the receiver's cell holds its waker
    rxWoken = FALSE,         \* the receiver's waker was called, not yet used
    txRegistered = FALSE,    \* the sender's cell holds its waker (a closed() poll)
    txWoken = FALSE,         \* the sender's waker was called, not yet used
    \* ---- the handles ----
    txAlive = TRUE,          \* the sender holds its half (until it sends or drops)
    rxAlive = TRUE,          \* the receiver holds its half (until it completes or drops)
    rxClose = "open",        \* the receiver's close note: "open", "withValue", "empty"
    \* ---- records the properties read; the code has no such variables ----
    valueWritten = FALSE,    \* the sender wrote the value into the cell
    cellReads = 0,           \* how many times the cell was read
    valueBack = FALSE,       \* a refused send took the value back
    rxValue = FALSE,         \* the receiver took the value out
    rxDropped = FALSE,       \* the receiver's drop destroyed the value
    sendOk = FALSE,          \* the send returned Ok
    sendRefused = FALSE,     \* the send returned Err: the receiver had closed first
    sendPastClose = FALSE,   \* a send answered Ok past a close (only a mutation gets here)
    blindRxWake = FALSE,     \* the receiver was woken with nothing visible (only a mutation)
    blindTxWake = FALSE,     \* the sender was woken with no close visible (no path does)
    rxReturnedValue = FALSE, \* a receive answered with the value
    rxReturnedError = FALSE, \* a receive answered with the error
    rxWasWoken = FALSE,      \* the receiver's waker was ever called
    txWasWoken = FALSE;      \* the sender's waker was ever called

define
end define

\* Wakes the receiver's task (`rx_waker.wake()`): the cell gives its waker
\* up and the task it belongs to is told.
macro wake_rx() begin
    if ~(valueSent \/ txDropped) then
        blindRxWake := TRUE;     \* (record) woken while nothing is visible to see
    end if;
    if rxRegistered then
        rxRegistered := FALSE;   \* the waker leaves the cell ...
        rxWoken := TRUE;         \* ... and the receiver's task is told
        rxWasWoken := TRUE;      \* (record) a wake was delivered
    end if;
end macro;

\* Wakes the sender's task (`tx_waker.wake()`): as above, for a sender that
\* waits in closed().
macro wake_tx() begin
    if ~rxClosed then
        blindTxWake := TRUE;     \* (record) woken while no close is visible
    end if;
    if txRegistered then
        txRegistered := FALSE;   \* the waker leaves the cell ...
        txWoken := TRUE;         \* ... and the sender's task is told
        txWasWoken := TRUE;      \* (record) a wake was delivered
    end if;
end macro;

(***************************************************************************)
(* The sender runs Prog, one operation after the other. "send" writes the  *)
(* value, publishes both bits in one fetch_or, and either wakes the        *)
(* receiver (Ok) or takes the value back (Err); either way the sender is   *)
(* spent. "drop" raises TX_DROPPED and wakes the receiver if it had not    *)
(* closed. "park" is one poll of closed(): look, register, look again,     *)
(* sleep until woken, look once more.                                      *)
(***************************************************************************)
fair process sender = Tx
variables i = 1;    \* the sender's operation
begin
S_next:
    \* The next operation, or the end of the program.
    if i > Len(Prog) then
        goto S_done;
    elsif Prog[i] = "send" then
        goto S_write;
    elsif Prog[i] = "drop" then
        goto S_drop;
    else
        goto P_look;               \* "park": one poll of closed()
    end if;
S_write:
    \* The value goes into the cell (`Inner::value`). The sender owns the
    \* cell until it publishes, so nobody else can be reading it yet.
    valueWritten := TRUE;
    valueInCell := TRUE;
    \* MUTATION wake_early: the send wakes the receiver before it publishes,
    \* a wake with nothing visible (and the wake after the publish still
    \* comes, so this is the double wake).
    if Mutation = "wake_early" then
        wake_rx();
    end if;
S_publish:
    \* One fetch_or(VALUE_SENT | TX_DROPPED): both bits go up together, and
    \* the word that came back says whether the receiver had closed.
    valueSent := TRUE;
    txDropped := TRUE;
    if rxClosed /\ Mutation # "send_ignores_close" then
        sendRefused := TRUE;       \* (record) the receiver closed first: Err
        goto S_back;
    else
        sendOk := TRUE;            \* (record) the value is the receiver's now
        if rxClosed then
            sendPastClose := TRUE; \* (record) only the mutation answers Ok here
        end if;
        goto S_wake;
    end if;
S_back:
    \* The failed send reads its value back out of the cell and returns it.
    cellReads := cellReads + 1;
    valueInCell := FALSE;
    valueBack := TRUE;
    txAlive := FALSE;              \* the sender is spent: send consumed it
    i := Len(Prog);                \* and no operation of the program is left
    goto S_op_done;
S_wake:
    \* The receiver's task is woken, then the send is done.
    \* MUTATION wake_lost: the wake never comes, so a sleeping receiver
    \* sleeps for good.
    if Mutation # "wake_lost" then
        wake_rx();
    end if;
    txAlive := FALSE;              \* the sender is spent: send consumed it
    i := Len(Prog);                \* and no operation of the program is left
    goto S_op_done;                \* a consumed sender has no drop left to run
S_drop:
    \* The sender handle's drop: one fetch_or(TX_DROPPED).
    txDropped := TRUE;
    txAlive := FALSE;              \* the handle is gone
    i := Len(Prog);                \* and no operation of the program is left
    \* A receiver that closed already has its answer without a wake; the
    \* wake_lost mutation drops the wake here too.
    if rxClosed \/ Mutation = "wake_lost" then
        goto S_op_done;
    else
        goto S_dwake;
    end if;
S_dwake:
    \* The receiver waits for exactly this: tell its task the sender is gone.
    wake_rx();
S_op_done:
    \* On to the next operation.
    i := i + 1;
    goto S_next;
P_look:
    \* `poll_closed`: a spent sender answers Ready at once, and so does a
    \* closed receiver.
    if ~txAlive \/ rxClosed then
        goto S_op_done;
    else
        goto P_register;
    end if;
P_register:
    \* Nothing closed yet: the waker goes into the sender's cell.
    txRegistered := TRUE;
    \* MUTATION tx_no_recheck: the sender sleeps without looking again.
    if Mutation = "tx_no_recheck" then
        goto P_sleep;
    end if;
P_recheck:
    \* The look after the registration: whatever came before it is seen here.
    if rxClosed then
        goto S_op_done;            \* closed meanwhile: Ready, no sleep needed
    else
        goto P_sleep;
    end if;
P_sleep:
    \* `Pending`: the sender's task sleeps until its waker is called.
    await txWoken;                 \* nothing happens to it until then
    txWoken := FALSE;              \* the wake is used up
    goto P_look;                   \* one more look, as a poll after a wake does
S_done:
    skip;                          \* the sender's program is over
end process;

(***************************************************************************)
(* The receiver runs RxProg. "recv" polls to completion (it may park);     *)
(* "try" is one try_recv (Empty keeps the handle, anything else ends the   *)
(* program, as a completed receiver answers nothing more: the code would   *)
(* panic, and no program here asks it); "close" closes; "drop" drops the   *)
(* handle, destroying a value it still owns.                               *)
(***************************************************************************)
fair process receiver = Rx
variables ri = 1;   \* the receiver's operation
begin
R_next:
    \* The next operation, or the end of the program.
    if ri > Len(RxProg) then
        goto R_done;
    elsif RxProg[ri] = "close" then
        goto R_close;
    elsif RxProg[ri] = "drop" then
        goto R_dclose;
    else
        goto R_look;               \* "recv" or "try": one look first
    end if;
R_look:
    \* One Acquire load of the word and, when there is something to see, the
    \* read that goes with it (`Inner::resolve`).
    if ~rxAlive then
        goto R_op_done;            \* a completed receiver answers nothing more
    elsif rxClose = "empty" then
        rxReturnedError := TRUE;   \* (record) closed with nothing sent: RecvError
        goto R_finish;
    elsif valueSent /\ Mutation # "error_despite_value" then
        cellReads := cellReads + 1;      \* the value leaves the cell ...
        valueInCell := FALSE;
        rxValue := TRUE;                 \* ... into the receiver's hands
        rxReturnedValue := TRUE;         \* (record) answered with the value
        \* MUTATION take_keeps_handle: the value is taken but the handle is
        \* not retired, so a later drop reads the cell a second time.
        if Mutation = "take_keeps_handle" then
            goto R_op_done;
        else
            goto R_finish;
        end if;
    elsif txDropped then
        rxReturnedError := TRUE;   \* (record) the sender left without sending
        goto R_finish;
    elsif RxProg[ri] = "try" then
        goto R_op_done;            \* try_recv: Empty, and the handle stays
    else
        goto R_register;           \* nothing yet: a recv will register
    end if;
R_register:
    \* The waker goes into the receiver's cell (`rx_waker.register`).
    rxRegistered := TRUE;
    \* MUTATION rx_no_recheck: the receiver sleeps without looking again.
    if Mutation = "rx_no_recheck" then
        goto R_sleep;
    end if;
R_recheck:
    \* The look after the registration: whatever came before it is seen here.
    if rxClose = "empty" then
        rxReturnedError := TRUE;   \* (record) closed with nothing sent
        goto R_finish;
    elsif valueSent /\ Mutation # "error_despite_value" then
        cellReads := cellReads + 1;      \* the value leaves the cell ...
        valueInCell := FALSE;
        rxValue := TRUE;                 \* ... into the receiver's hands
        rxReturnedValue := TRUE;         \* (record) answered with the value
        if Mutation = "take_keeps_handle" then
            goto R_op_done;        \* MUTATION take_keeps_handle: handle kept
        else
            goto R_finish;
        end if;
    elsif txDropped then
        rxReturnedError := TRUE;   \* (record) the sender left without sending
        goto R_finish;
    else
        goto R_sleep;              \* still nothing: sleep until woken
    end if;
R_sleep:
    \* `Pending`: the receiver's task sleeps until its waker is called.
    await rxWoken;                 \* nothing happens to it until then
    rxWoken := FALSE;              \* the wake is used up
    goto R_look;                   \* the poll after a wake starts at the look
R_close:
    \* `Receiver::close`: one fetch_or(RX_CLOSED) and the note of what it saw.
    if ~rxAlive \/ rxClose # "open" then
        goto R_op_done;            \* completed, or closed already: nothing to do
    else
        rxClosed := TRUE;          \* the bit goes up ...
        rxClose := IF valueSent THEN "withValue" ELSE "empty";  \* ... with the note
        if ~txDropped then
            wake_tx();             \* a live sender waiting in closed() is told
        end if;
        goto R_op_done;
    end if;
R_dclose:
    \* The receiver handle's drop: the same fetch_or when the note is still
    \* open (a closed receiver already did it), then the value below.
    if ~rxAlive then
        goto R_op_done;            \* completed already: the handle is empty
    elsif rxClose = "open" then
        rxClosed := TRUE;          \* the bit goes up ...
        rxClose := IF valueSent THEN "withValue" ELSE "empty";  \* ... with the note
        if ~txDropped then
            wake_tx();             \* a live sender waiting in closed() is told
        end if;
        goto R_dvalue;
    else
        goto R_dvalue;             \* closed before: the note already says it
    end if;
R_dvalue:
    \* A note of "withValue" means the cell is the receiver's: the value is
    \* read out and destroyed. An "empty" note never owned it.
    rxAlive := FALSE;              \* the handle is gone
    \* MUTATION drop_loses_value: the drop throws the value away unread.
    if rxClose = "withValue" /\ Mutation # "drop_loses_value" then
        cellReads := cellReads + 1;      \* the value leaves the cell ...
        valueInCell := FALSE;
        rxDropped := TRUE;         \* ... and is destroyed on the spot
    end if;
R_finish:
    \* The receiver completed: it answered, and its handle is retired.
    rxAlive := FALSE;
    ri := Len(RxProg);             \* nothing of the program is left
R_op_done:
    \* On to the next operation.
    ri := ri + 1;
    goto R_next;
R_done:
    skip;                          \* the receiver's program is over
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties. Each says one thing the channel promises.               *)
(***************************************************************************)

\* Where the value is, as a count: in the cell, back with a refused send,
\* taken by the receiver, or destroyed by the receiver's drop.
ValuePlaces ==
    (IF valueInCell THEN 1 ELSE 0) + (IF valueBack THEN 1 ELSE 0)
    + (IF rxValue THEN 1 ELSE 0) + (IF rxDropped THEN 1 ELSE 0)

\* The value is dropped exactly once: once written it is in exactly one
\* place, and once both halves are gone it is out of the cell (so a value
\* nobody received was destroyed, not leaked).
ValueConserved ==
    /\ (valueWritten => ValuePlaces = 1)
    /\ (~txAlive /\ ~rxAlive => ~valueInCell)

\* No value is observed twice: the cell is read at most once, by whoever
\* owns it (a receive, the receiver's drop, or a refused send's read-back).
ValueReadOnce == cellReads <= 1

\* Every wake is earned: the receiver is woken only when the word shows a
\* value or a gone sender, and a sender waiting in closed() only when the
\* word shows the close. (The tx half cannot go blind in any run: its wake
\* always shares the step that raises RX_CLOSED; the clause is checked
\* anyway.)
WakeMeansVisible == ~blindRxWake /\ ~blindTxWake

\* A send that races a closed receiver fails and the value comes back: no
\* send ever answers Ok past a close (the API's "a second send fails", in
\* the form it can be asked: the method consumes the sender, so the race
\* with a close is the only refusal there is).
NoSendPastClose == ~sendPastClose

\* The receiver's answer matches the word: a value only after a successful
\* send, an error only when no send succeeded (so nothing was left behind).
AnswerSound ==
    /\ (rxReturnedValue => sendOk)
    /\ (rxReturnedError => ~sendOk)

\* All the safety properties together.
Safe == ValueConserved /\ ValueReadOnce /\ WakeMeansVisible
        /\ NoSendPastClose /\ AnswerSound

(***************************************************************************)
(* Witnesses. Each one claims an interesting case never happens; a         *)
(* configuration checks it and expects it to break, which proves the       *)
(* search really reaches that case.                                        *)
(***************************************************************************)

\* The value lands while the receiver is between its registration and its
\* re-check: the lost-wake window, which the re-check closes.
NoValueAtRecheck == pc[Rx] = "R_recheck" => ~valueSent
\* The receiver drops between the send's publish and its wake: the wake
\* then finds nobody, and the drop itself owns the value.
NoWakeRacingDrop == pc[Tx] = "S_wake" => pc[Rx] \notin {"R_dclose", "R_dvalue"}
\* A send into a closed receiver comes back with the value.
NoSendRefused == ~sendRefused
\* A parked receiver is woken by the send.
NoRxWake == ~rxWasWoken
\* A sender parked in closed() is woken by the receiver's close.
NoTxWake == ~txWasWoken
\* A value nobody received is destroyed by the receiver's drop.
NoValueDroppedByRx == ~rxDropped
=============================================================================

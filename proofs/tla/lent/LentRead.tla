----------------------------- MODULE LentRead -----------------------------
(***************************************************************************)
(* A stream read that lends the side the task's own buffer                 *)
(* (src/io/source/read.rs: `read_owned`, `read_lent`, `abandon_lent`).     *)
(*                                                                          *)
(* In plain words: bytes arrive in the kernel in order, each with an edge  *)
(* for the driver, and then the stream ends (one more edge). The lending task hands its buffer, with room for Cap    *)
(* bytes, to the side; the driver reads into it at once if the socket may  *)
(* hold bytes, or on a later edge, and wakes the task, which takes the     *)
(* buffer back with the bytes. While its buffer is lent the task may go   *)
(* away (its future dropped): what the driver read into the buffer then   *)
(* becomes the read-ahead. Another task reads as usual, into the pool's   *)
(* read-ahead, until it reads the end. While a filled buffer waits to be  *)
(* taken back, the driver reads nothing more.                              *)
(*                                                                          *)
(* What is abstract: the side's guard (a step under it runs whole here;   *)
(* Side.tla checks the guard and the hand-off of the driver's part to a   *)
(* holder), and how much one call reads (all the kernel holds, up to the  *)
(* room of the buffer it reads into).                                      *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    Total,     \* the bytes that arrive, numbered 1..Total
    Cap,       \* the room of the lent buffer
    MayLeave,  \* whether the lending task may go away while its buffer is lent
    Mutation   \* "none", or the name of the one rule this run takes away

(* --algorithm LentRead
variables
    holder = "none",      \* who holds the side
    kernel = <<>>,        \* bytes the kernel holds, oldest first
    arrived = 0,
    closed = FALSE,       \* the stream ended: a read of an empty kernel reads the end
    edges = 0,            \* edges not yet handed to the driver
    blocked = FALSE,      \* the readiness cache
    lent = "none",        \* the lent buffer: "none", "waiting" for the driver, or "done"
    inLent = <<>>,        \* the bytes the driver read into it
    bufAt = "task",       \* who has the lender's buffer: "task", "side" or "gone"
    ahead = <<>>,         \* the read-ahead
    eof = FALSE,          \* the end, read ahead
    lenderWoken = FALSE,
    readerWaiting = FALSE,
    readerWoken = FALSE,
    lenderGot = <<>>,     \* bytes the lending task took, in order
    readerGot = <<>>,     \* bytes the other task took, in order
    scribbled = FALSE,    \* (record) the driver read into a buffer the side did not have
    driverFilled = FALSE, \* (record) the driver filled the lent buffer on an edge
    submitFilled = FALSE, \* (record) the lender's own poll filled it
    leftBytes = FALSE;    \* (record) a task that went away left bytes to the read-ahead

define
    Min(a, b) == IF a < b THEN a ELSE b
    Taken == Len(lenderGot) + Len(readerGot)
    AllDone == pc["kernel"] = "Done" /\ pc["lender"] = "Done" /\ pc["reader"] = "Done"
end define;

\* The driver's read (`complete`), under the guard. `edge`: an edge clears the cache. Into the
\* lent buffer while it waits, else into the read-ahead; nothing while a result waits.
macro Complete(who, edge) begin
    \* MUTATION read_past_lent: a filled lent buffer does not stop the reads.
    if (edge \/ ~blocked) /\ ahead = <<>> /\ ~eof /\ (lent # "done" \/ Mutation = "read_past_lent") then
        if kernel = <<>> /\ ~closed then
            blocked := TRUE;
        elsif kernel = <<>> then
            \* The end, for the lent buffer if one waits.
            blocked := FALSE;
            if lent = "waiting" then
                lent := "done";
                if who # "lender" then lenderWoken := TRUE; end if;
            else
                eof := TRUE;
            end if;
        elsif lent = "waiting" then
            blocked := FALSE;
            if bufAt # "side" then scribbled := TRUE; end if;
            inLent := SubSeq(kernel, 1, Min(Cap, Len(kernel)));
            kernel := SubSeq(kernel, Min(Cap, Len(kernel)) + 1, Len(kernel));
            lent := "done";
            if who = "driver" then driverFilled := TRUE; else submitFilled := TRUE; end if;
            \* MUTATION no_wake: the read fills the buffer and wakes nobody.
            if who # "lender" /\ Mutation # "no_wake" then lenderWoken := TRUE; end if;
        else
            blocked := FALSE;
            ahead := kernel;
            kernel := <<>>;
        end if;
    elsif edge then
        blocked := FALSE;
    end if;
end macro;

fair process kernelP = "kernel"
begin
K_arrive:
    while arrived < Total do
        arrived := arrived + 1;
        kernel := Append(kernel, arrived);
        edges := edges + 1;
    end while;
K_close:
    closed := TRUE;
    edges := edges + 1;
end process;

fair process driver = "driver"
begin
D_loop:
    while TRUE do
        await edges > 0 \/ (AllDone /\ edges = 0);
        if edges = 0 then goto Done; end if;
D_hold:
        await holder = "none";
        holder := "driver";
        edges := edges - 1;
D_serve:
        if readerWaiting \/ lent = "waiting" then
            Complete("driver", TRUE);
        else
            blocked := FALSE;
        end if;
D_wake:
        if readerWaiting /\ (ahead # <<>> \/ eof) then
            readerWoken := TRUE; readerWaiting := FALSE;
        end if;
        holder := "none";
    end while;
end process;

fair process lender = "lender"
begin
L_hold:
    await holder = "none";
    holder := "lender";
L_step:
    \* What another read left (bytes, or the end) is taken first; else the buffer is lent.
    if ahead # <<>> then
        lenderGot := SubSeq(ahead, 1, Min(Cap, Len(ahead)));
        ahead := SubSeq(ahead, Min(Cap, Len(ahead)) + 1, Len(ahead));
        if readerWaiting /\ ahead # <<>> then readerWoken := TRUE; readerWaiting := FALSE; end if;
        holder := "none";
        goto Done;
    elsif eof then
        eof := FALSE;
        holder := "none";
        goto Done;
    else
        lent := "waiting";
        bufAt := "side";
    end if;
L_submit:
    Complete("lender", FALSE);
L_check:
    if lent = "done" then
        lenderGot := inLent;
        inLent := <<>>;
        \* MUTATION take_keeps_slot: the buffer is taken back but the side still counts it lent.
        lent := IF Mutation = "take_keeps_slot" THEN "waiting" ELSE "none";
        bufAt := "task";
        if readerWaiting then readerWoken := TRUE; readerWaiting := FALSE; end if;
        holder := "none";
        goto Done;
    else
        holder := "none";
    end if;
L_wait:
    either
        await lenderWoken;
        lenderWoken := FALSE;
        goto L_rehold;
    or
        await MayLeave;
        goto L_leave;
    end either;
L_rehold:
    await holder = "none";
    holder := "lender";
L_again:
    Complete("lender", FALSE);
    goto L_check;
L_leave:
    await holder = "none";
    holder := "lender";
L_abandon:
    \* The task went away: the bytes read into its buffer are the read-ahead now.
    bufAt := "gone";
    if lent = "done" /\ inLent = <<>> then
        eof := TRUE;
    elsif lent = "done" then
        \* MUTATION abandon_drops: the bytes go with the buffer.
        if Mutation # "abandon_drops" then
            ahead := inLent;
            leftBytes := TRUE;
        end if;
        inLent := <<>>;
    end if;
    lent := "none";
    if readerWaiting then readerWoken := TRUE; readerWaiting := FALSE; end if;
    holder := "none";
end process;

fair process reader = "reader"
begin
R_loop:
    while TRUE do
R_hold:
        await holder = "none";
        holder := "reader";
R_step:
        Complete("reader", FALSE);
R_take:
        if ahead # <<>> then
            readerGot := readerGot \o ahead;
            ahead := <<>>;
            holder := "none";
        elsif eof then
            \* A read takes the end.
            eof := FALSE;
            holder := "none";
            goto Done;
        else
            readerWaiting := TRUE;
            holder := "none";
R_sleep:
            await readerWoken;
            readerWoken := FALSE;
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

Increasing(s) == \A i \in 1..(Len(s) - 1) : s[i] < s[i + 1]

\* No byte is lost or made up: each is in the kernel, in the lent buffer, read ahead, or taken.
Conservation == Taken + Len(inLent) + Len(ahead) + Len(kernel) = arrived

\* Each task gets the bytes in the order they came.
Order == Increasing(lenderGot) /\ Increasing(readerGot)

\* The driver reads only into a buffer the side has.
NoScribble == ~scribbled

\* Once both tasks are done, every byte was taken.
AllTaken == AllDone => Taken = Total

\* `Termination` (every process ends, so every byte was taken) is the one the translator writes
\* for the fair processes.

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* The driver fills the lent buffer on an edge.
NoDriverFill == ~driverFilled
\* The lender's own poll fills its buffer at once.
NoSubmitFill == ~submitFilled
\* A task that went away leaves the bytes of its buffer to the read-ahead.
NoLeftBytes == ~leftBytes
=============================================================================

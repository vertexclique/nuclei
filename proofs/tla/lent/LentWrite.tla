----------------------------- MODULE LentWrite -----------------------------
(***************************************************************************)
(* A stream write that lends the side the task's own buffer                *)
(* (src/io/source/write/lent.rs: `write_owned`, `push_lent`,               *)
(* `abandon_lent`).                                                        *)
(*                                                                          *)
(* In plain words: the socket's buffer is full and byte 1 waits in the    *)
(* write-behind. The lending task hands the side its buffer, bytes 2 to 4; *)
(* the driver writes it out after the write-behind, as the peer makes room *)
(* (each byte the peer reads raises an edge), and wakes the task once the *)
(* kernel took the last byte; the task then takes the buffer back. A later *)
(* copied write, byte 5, waits until the lent buffer is out. While its    *)
(* buffer is lent the task may go away (its future dropped): a write none *)
(* of which went out is withdrawn, and one that started is finished by the *)
(* driver, which then drops the buffer. So the wire carries 1 2 3 4 5, or  *)
(* 1 5, and never a part of the lent buffer.                               *)
(*                                                                          *)
(* What is abstract: the side's guard (a step under it runs whole here;   *)
(* Side.tla checks it), and how much one call writes (all the socket has  *)
(* room for).                                                              *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    Room,      \* the bytes the socket's buffer holds
    MayLeave,  \* whether the lending task may go away while its buffer is lent
    Mutation   \* "none", or the name of the one rule this run takes away

Lent == <<2, 3, 4>>
Whole == <<1, 2, 3, 4, 5>>
Without == <<1, 5>>

(* --algorithm LentWrite
variables
    holder = "none",      \* who holds the side
    kbuf = Room,          \* bytes in the socket's buffer: full at the start
    wire = <<>>,          \* bytes the kernel took, in order
    edges = 0,            \* writable edges not yet handed to the driver
    blocked = TRUE,       \* the readiness cache: the last write would have blocked
    behind = <<1>>,       \* the write-behind
    lent = "none",        \* the lent buffer: "none", "waiting" with bytes left, or "done"
    sent = 0,             \* bytes of it the kernel took
    orphan = FALSE,       \* its task went away after some of it went out
    bufAt = "task",       \* who has the lender's buffer: "task", "side" or "gone"
    lendStarted = FALSE,
    moved = FALSE,        \* the last push changed something
    lenderWoken = FALSE,
    writerWaiting = FALSE,
    writerWoken = FALSE,
    scribbled = FALSE,    \* (record) the driver wrote from a buffer the side did not have
    partial = FALSE,      \* (record) the lent buffer went out over more than one call
    orphanDone = FALSE,   \* (record) the driver finished a write whose task went away
    withdrawn = FALSE;    \* (record) a write none of which went out was withdrawn

define
    Min(a, b) == IF a < b THEN a ELSE b
    IsPrefix(s, t) == Len(s) <= Len(t) /\ SubSeq(t, 1, Len(s)) = s
    WritersDone == pc["lender"] = "Done" /\ pc["writer"] = "Done"
    Pending == behind # <<>> \/ lent = "waiting"
    AllDone == WritersDone /\ pc["peer"] = "Done"
end define;

\* The driver's writes (`push`), under the guard: the write-behind first, then the lent buffer,
\* while the socket has room. `edge`: an edge clears the cache.
macro Push(who, edge) begin
    if edge \/ ~blocked then
        with n1 = Min(Room - kbuf, Len(behind)) do
        with goLent = n1 = Len(behind) /\ lent = "waiting" do
        with n2 = IF goLent THEN Min(Room - kbuf - n1, Len(Lent) - sent) ELSE 0 do
        with total = sent + n2 do
            \* Each value below is computed from the state before the push.
            blocked := n1 < Len(behind) \/ (goLent /\ total < Len(Lent));
            moved := n1 + n2 > 0;
            wire := wire \o SubSeq(behind, 1, n1) \o SubSeq(Lent, sent + 1, total);
            kbuf := kbuf + n1 + n2;
            behind := SubSeq(behind, n1 + 1, Len(behind));
            if goLent then
                if n2 > 0 /\ bufAt # "side" then scribbled := TRUE; end if;
                if n2 > 0 /\ total < Len(Lent) then partial := TRUE; end if;
                sent := total;
                if total = Len(Lent) then
                    if orphan then
                        \* Nobody waits: the side drops the buffer.
                        lent := "none";
                        bufAt := "gone";
                        orphanDone := TRUE;
                    else
                        lent := "done";
                        \* MUTATION no_wake: the write ends and wakes nobody.
                        if who # "lender" /\ Mutation # "no_wake" then lenderWoken := TRUE; end if;
                    end if;
                end if;
            end if;
        end with;
        end with;
        end with;
        end with;
    else
        moved := FALSE;
    end if;
end macro;

fair process peer = "peer"
begin
P_loop:
    while ~(WritersDone /\ kbuf = 0 /\ ~Pending) do
        await kbuf > 0;
        kbuf := kbuf - 1;
        edges := edges + 1;
    end while;
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
        Push("driver", TRUE);
D_wake:
        if writerWaiting /\ moved then
            writerWoken := TRUE; writerWaiting := FALSE;
        end if;
        holder := "none";
    end while;
end process;

fair process lender = "lender"
begin
L_hold:
    await holder = "none";
    holder := "lender";
L_lend:
    lent := "waiting";
    bufAt := "side";
    lendStarted := TRUE;
L_submit:
    Push("lender", FALSE);
L_check:
    \* MUTATION take_early: the task takes its buffer back once some of it went out.
    if lent = "done" \/ (Mutation = "take_early" /\ sent > 0 /\ ~orphan) then
        bufAt := "task";
        if lent = "done" then lent := "none"; end if;
        if writerWaiting then writerWoken := TRUE; writerWaiting := FALSE; end if;
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
    Push("lender", FALSE);
    goto L_check;
L_leave:
    await holder = "none";
    holder := "lender";
L_abandon:
    \* MUTATION withdraw_started: a write that started is withdrawn too.
    if lent = "waiting" /\ sent > 0 /\ Mutation # "withdraw_started" then
        orphan := TRUE;
    else
        if lent = "waiting" /\ sent = 0 then withdrawn := TRUE; end if;
        lent := "none";
        bufAt := "gone";
        if writerWaiting then writerWoken := TRUE; writerWaiting := FALSE; end if;
    end if;
    holder := "none";
end process;

fair process writer = "writer"
begin
W_start:
    \* A copied write that comes after the lent one.
    await lendStarted;
W_hold:
    await holder = "none";
    holder := "writer";
W_step:
    \* MUTATION copy_passes_lent: a copied write does not wait for the lent buffer.
    if lent = "waiting" /\ Mutation # "copy_passes_lent" then
        writerWaiting := TRUE;
        holder := "none";
W_sleep:
        await writerWoken;
        writerWoken := FALSE;
        goto W_hold;
    else
        behind := behind \o <<5>>;
    end if;
W_push:
    Push("writer", FALSE);
    holder := "none";
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

\* The wire carries the bytes in order, the lent buffer whole or not at all.
WireOrder == IsPrefix(wire, Whole) \/ IsPrefix(wire, Without)

\* Once the peer read everything, every byte that was written is on the wire.
Delivered == pc["peer"] = "Done" => wire \in {Whole, Without}

\* The driver writes only from a buffer the side has.
NoScribble == ~scribbled

\* `Termination` (every process ends, so the peer read everything) is the one the translator
\* writes for the fair processes.

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* The lent buffer goes out over more than one call.
NoPartial == ~partial
\* The driver finishes the write of a task that went away.
NoOrphanDone == ~orphanDone
\* A write none of which went out is withdrawn.
NoWithdraw == ~withdrawn
=============================================================================

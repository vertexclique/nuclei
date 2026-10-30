----------------------------- MODULE RingLent -----------------------------
(***************************************************************************)
(* A task's own buffer written through the ring (`write_owned`, in        *)
(* src/io/source/write/ops.rs), whose task may go away while the buffer   *)
(* is out with the kernel.                                                 *)
(*                                                                          *)
(* In plain words: the buffer is out with a send. The kernel takes some of *)
(* its bytes and completes, or, if a cancel came before it took any,      *)
(* completes cancelled with none. The ring's thread applies each          *)
(* completion: the bytes taken count, and what is left goes out next. The *)
(* task may go away at any time; if nothing of the buffer went out yet    *)
(* and the send is out, its going asks the kernel to cancel it. A         *)
(* completion that leaves an abandoned buffer with nothing sent withdraws *)
(* it; one that leaves part of it sent goes on to the end. The source may *)
(* also move to another shard while the send is out: the send is          *)
(* cancelled, and what is left goes out from the new home once its end    *)
(* comes. So the stream gets the buffer whole or not at all.             *)
(*                                                                          *)
(* A completion port's TCP send (src/io/source/port.rs, `may_cancel`) is  *)
(* not that honest: cancelled after the stack took some of its bytes, it  *)
(* may still come back cancelled with none counted. So there a TCP send   *)
(* is never cancelled: an abandoned one, or one whose source moves, ends  *)
(* on its own.                                                             *)
(*                                                                          *)
(* What is abstract: the side's guard (the ring's thread applies every    *)
(* completion; RingOp.tla checks the hand-off), and the bytes, `Len`     *)
(* units of which the kernel takes one or more at a time.                  *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Port,      \* TRUE: the send is a completion port's TCP send
    Mutation   \* "none", or the name of the one rule this run takes away

Len == 2

\* MUTATION cancel_tcp: a completion port's TCP send is cancelled too.
MayCancel == ~Port \/ Mutation = "cancel_tcp"

(* --algorithm RingLent
variables
    sent = 0,             \* units of the buffer the completions counted
    wire = 0,             \* units the stream got, counted or not
    out = TRUE,           \* a send of the rest is out with the kernel
    took = -1,            \* what the kernel's last completion took (-1: none waits)
    cancelAsked = FALSE,  \* the task's going asked the kernel to cancel the send
    orphan = FALSE,       \* the task went
    whole = FALSE,        \* every unit went out, and the write ended
    withdrawn = FALSE,    \* the write was withdrawn, none of it out
    moved = FALSE,        \* the source moved to another shard
    resent = FALSE;       \* (record) a send cancelled as its source moved went out again

fair process kernel = "kernel"
begin
K_loop:
    while ~(whole \/ withdrawn) do
        await out;
        either
            \* It takes one or more of the units left.
            with n \in 1..(Len - sent) do
                wire := wire + n;
                took := n;
            end with;
        or
            \* A cancel that came before it took any: it completes cancelled.
            await cancelAsked;
            took := 0;
        or
            \* A completion port's TCP send cancelled after the stack took some
            \* of the units: it completes cancelled with none counted.
            await Port /\ cancelAsked;
            with n \in 1..(Len - sent) do
                wire := wire + n;
                took := 0;
            end with;
        end either;
        out := FALSE;
        cancelAsked := FALSE;
K_applied:
        await took = -1 \/ whole \/ withdrawn;
    end while;
end process;

\* The ring's thread applies each completion.
fair process ring = "ring"
begin
R_loop:
    while ~(whole \/ withdrawn) do
        await took >= 0;
        if moved /\ took = 0 /\ ~orphan then resent := TRUE; end if;
        sent := sent + took;
        took := -1;
        if sent = Len then
            whole := TRUE;
        \* MUTATION withdraw_started: a buffer whose task went is withdrawn though part went out.
        elsif orphan /\ (sent = 0 \/ Mutation = "withdraw_started") then
            withdrawn := TRUE;
        \* MUTATION no_resubmit: what is left of an abandoned buffer is not sent.
        elsif ~(orphan /\ Mutation = "no_resubmit") then
            out := TRUE;
        end if;
    end while;
end process;

\* The task goes away, at some point, or never.
fair process task = "task"
begin
T_go:
    either
        skip;
    or
        await ~(whole \/ withdrawn);
        orphan := TRUE;
        \* Nothing went out and the send is out: the kernel is asked to cancel it.
        if out /\ sent = 0 /\ MayCancel then
            cancelAsked := TRUE;
        end if;
    end either;
end process;

\* The source moves to another shard, at some point, or never: the send out
\* is cancelled, and its end comes wherever the completions go then.
fair process mover = "mover"
begin
M_go:
    either
        skip;
    or
        await out /\ ~(whole \/ withdrawn);
        moved := TRUE;
        if MayCancel then
            cancelAsked := TRUE;
        end if;
    end either;
end process;
end algorithm; *)
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* The write ends: whole, or withdrawn.
Settles == <>(whole \/ withdrawn)

\* The stream gets the buffer whole or not at all, and each unit once.
WholeOrNothing == (whole => wire = Len) /\ (withdrawn => wire = 0) /\ wire <= Len

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A write is withdrawn: the cancel came before the kernel took anything.
NoWithdraw == ~withdrawn
\* An abandoned write goes on to the end after part of it went out.
NoOrphanFinish == ~(orphan /\ whole /\ sent = Len)
\* A send cancelled as its source moved goes out again from the new home.
NoResend == ~resent
=============================================================================

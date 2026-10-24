----------------------------- MODULE RingLent -----------------------------
(***************************************************************************)
(* A task's own buffer written through the ring (`write_owned`, in        *)
(* src/io/source/write/ring.rs), whose task may go away while the buffer  *)
(* is out with the kernel.                                                 *)
(*                                                                          *)
(* In plain words: the buffer is out with a send. The kernel takes some of *)
(* its bytes and completes, or, if a cancel came before it took any,      *)
(* completes cancelled with none. The ring's thread applies each          *)
(* completion: the bytes taken count, and what is left goes out next. The *)
(* task may go away at any time; if nothing of the buffer went out yet    *)
(* and the send is out, its going asks the kernel to cancel it. A         *)
(* completion that leaves an abandoned buffer with nothing sent withdraws *)
(* it; one that leaves part of it sent goes on to the end. So the stream  *)
(* gets the buffer whole or not at all.                                    *)
(*                                                                          *)
(* What is abstract: the side's guard (the ring's thread applies every    *)
(* completion; RingOp.tla checks the hand-off), and the bytes, `Len`     *)
(* units of which the kernel takes one or more at a time.                  *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Mutation   \* "none", or the name of the one rule this run takes away

Len == 2

(* --algorithm RingLent
variables
    sent = 0,             \* units of the buffer the kernel took
    out = TRUE,           \* a send of the rest is out with the kernel
    took = -1,            \* what the kernel's last completion took (-1: none waits)
    cancelAsked = FALSE,  \* the task's going asked the kernel to cancel the send
    orphan = FALSE,       \* the task went
    whole = FALSE,        \* every unit went out, and the write ended
    withdrawn = FALSE;    \* the write was withdrawn, none of it out

fair process kernel = "kernel"
begin
K_loop:
    while ~(whole \/ withdrawn) do
        await out;
        either
            \* It takes one or more of the units left.
            with n \in 1..(Len - sent) do
                took := n;
            end with;
        or
            \* A cancel that came before it took any: it completes cancelled.
            await cancelAsked /\ sent = 0;
            took := 0;
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
        if out /\ sent = 0 then
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

\* The stream gets the buffer whole or not at all.
WholeOrNothing == (whole => sent = Len) /\ (withdrawn => sent = 0)

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A write is withdrawn: the cancel came before the kernel took anything.
NoWithdraw == ~withdrawn
\* An abandoned write goes on to the end after part of it went out.
NoOrphanFinish == ~(orphan /\ whole /\ sent = Len)
=============================================================================

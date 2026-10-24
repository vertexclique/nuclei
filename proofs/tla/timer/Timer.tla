------------------------------- MODULE Timer -------------------------------
(***************************************************************************)
(* A timer a shard fires for a thread that runs no shard                  *)
(* (src/time/sleep.rs: `RemoteTimer`, `TimerOp`), at the grain of its      *)
(* atomic steps.                                                           *)
(*                                                                          *)
(* In plain words: a sleep polled on another runtime's thread cannot touch *)
(* a shard's timer wheel, so it posts an arm request into the shard's      *)
(* mailbox and waits on a small shared cell: a `fired` flag and a waker    *)
(* slot. The shard applies its mailbox in order, arms the timer in its     *)
(* wheel, and when the deadline passes sets `fired`, wakes the waker and   *)
(* removes the entry. Dropping the sleep marks the cell cancelled, takes   *)
(* the waker out, and posts a cancel; the shard removes the entry if it    *)
(* still has it. Nothing is left behind whichever comes first.             *)
(*                                                                          *)
(* What is modelled step by step: the post of each op, the shard's apply   *)
(* of each op, the fire, the cell's flag, the waker slot (register, look   *)
(* again, take, wake), the poll's register-then-look-again, and the drop.  *)
(* What is abstract: time is one flag that turns on when the deadline     *)
(* passes (or, for a dropped sleep, may never within the run); the waker   *)
(* slot is one cell with one task's waker. Its register-then-look-again    *)
(* handshake guards a waker that changed between polls, which one task     *)
(* cannot show: that handshake is checked by the loom models of            *)
(* nuclei::sync's waker cell.                                              *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    DropIt,    \* TRUE: the sleep is dropped (at some point); FALSE: it is awaited to the end
    Mutation   \* "none", or the name of the one rule this run takes away

ForeignT == 1
ShardT == 2
ClockT == 3

(* --algorithm Timer
variables
    mailbox = <<>>,     \* the shard's mailbox: ops in the order they were posted
    entry = "none",     \* the wheel entry: "none", "armed" or "fired" (fired, not removed yet)
    keyStored = FALSE,  \* the cell knows the entry's key (written by the shard)
    fired = FALSE,      \* the cell's `fired` flag
    cancelled = FALSE,  \* the cell's `cancelled` flag
    slot = FALSE,       \* the cell's waker slot holds the foreign task's waker
    passed = FALSE,     \* the deadline has passed
    woken = FALSE,      \* the foreign task's waker was woken
    alive = TRUE,       \* the sleep exists
    \* records
    wokeDead = FALSE,   \* a wake reached a task whose sleep was dropped
    leftEntry = FALSE;  \* (computed at the end) an entry stayed in the wheel

define
    AllDone == pc[ForeignT] = "Done" /\ pc[ShardT] = "Done" /\ pc[ClockT] = "Done"
end define;

fair process clock = ClockT
begin
C_pass:
    \* An awaited sleep's deadline passes. A dropped one's may be far off and never come
    \* within the run: then only the cancel can remove its entry.
    if DropIt then
        either passed := TRUE; or skip; end either;
    else
        passed := TRUE;
    end if;
end process;

fair process foreign = ForeignT
begin
F_poll:
    \* First poll: register the waker in the cell, then post the arm request.
    slot := TRUE;
    mailbox := Append(mailbox, "insert");
F_wait:
    \* Await the fire, or drop the sleep.
    either
        await DropIt;
        goto F_drop;
    or
        await woken \/ (DropIt = FALSE /\ fired);
        goto F_poll_again;
    end either;
F_poll_again:
    \* `poll_cell`: look at `fired`; if not set, register and look again.
    if fired then
        goto F_done;
    else
        slot := TRUE;
        woken := FALSE;
    end if;
F_recheck:
    \* Look again after registering: a fire in between found the old waker.
    if fired then
        goto F_done;
    else
        await woken;
        goto F_poll_again;
    end if;
F_drop:
    \* `release`: mark cancelled, take the waker out, post the cancel.
    cancelled := TRUE;
    \* MUTATION drop_keeps_waker: the drop leaves the waker in the cell.
    if Mutation # "drop_keeps_waker" then slot := FALSE; end if;
    alive := FALSE;
    mailbox := Append(mailbox, "cancel");
F_done:
    skip;
end process;

fair process shard = ShardT
begin
S_loop:
    \* The shard's look at its timers: apply the mailbox, then fire what is due. It stops
    \* once the sleep and the clock are done and nothing is left to do (the model's end).
    if mailbox # <<>> then
        goto S_apply;
    elsif entry = "armed" /\ passed then
        goto S_fire;
    elsif pc[ForeignT] = "Done" /\ pc[ClockT] = "Done" then
        goto Done;
    else
        await mailbox # <<>> \/ (entry = "armed" /\ passed)
              \/ (pc[ForeignT] = "Done" /\ pc[ClockT] = "Done");
        goto S_loop;
    end if;
S_apply:
    \* One op, in order.
    if Head(mailbox) = "insert" then
        if ~cancelled then
            entry := "armed";
            keyStored := TRUE;
        end if;
    else
        \* "cancel": remove the entry if the cell knows it.
        \* MUTATION cancel_keeps_entry: the cancel does not remove it.
        if keyStored /\ Mutation # "cancel_keeps_entry" then
            entry := "none";
            keyStored := FALSE;
        end if;
    end if;
    mailbox := Tail(mailbox);
    goto S_loop;
S_fire:
    \* The wheel fires the entry; the cell's wake removes it, sets `fired`, wakes the waker.
    \* MUTATION fire_keeps_entry: the fired entry stays in the wheel.
    if Mutation = "fire_keeps_entry" then
        entry := "fired";
    else
        entry := "none";
    end if;
    keyStored := FALSE;
    fired := TRUE;
S_wake:
    if slot then
        slot := FALSE;
        woken := TRUE;
        if ~alive then wokeDead := TRUE; end if;
    end if;
    goto S_loop;
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

\* Once everything is done, nothing of the timer is left in the wheel.
NothingLeft == AllDone => entry = "none"

\* No wake reaches the task of a sleep that was dropped.
NoWakeAfterDrop == ~wokeDead

Safe == NothingLeft /\ NoWakeAfterDrop

(***************************************************************************)
(* Witnesses.                                                              *)
(***************************************************************************)

\* The sleep is dropped after its timer was armed.
NoCancelOfArmed == ~(cancelled /\ entry = "armed")
\* The sleep is dropped before the shard applied the arm request.
NoCancelBeforeArm == ~(cancelled /\ mailbox = <<"insert", "cancel">>)
\* The timer fires and wakes the waiting task.
NoFireWake == ~(fired /\ woken)
=============================================================================

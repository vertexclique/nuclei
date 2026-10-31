------------------------------- MODULE Timer -------------------------------
(***************************************************************************)
(* A timer a shard fires for a thread that runs no shard                  *)
(* (src/time/sleep.rs: `RemoteTimer`, `Armed`, `TimerOp`, `Sleep::poll`;   *)
(* src/rt/shard.rs: `Shared::post_timer`; src/rt/shard/going.rs: the       *)
(* shard's drop), at the grain of its atomic steps.                        *)
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
(* The shard may go before the deadline (its runtime dropped, a worker     *)
(* stopped): then nothing will ever fire the timer it holds. As it goes it *)
(* lets go of its wheel's entries and drains its mailbox, and every cell   *)
(* of a timer it held (armed, or asked for and not armed yet) is orphaned: *)
(* marked so and its waker woken, so the cell never keeps the task's waker *)
(* for good (the task holds the cell: kept, the two keep each other). A    *)
(* request posted to a shard that went is drained by the poster at once,   *)
(* the same way. The sleep, polled again, finds its cell orphaned, lets it *)
(* go and arms the timer again where it is polled: here, on the second     *)
(* shard, which never goes.                                                *)
(*                                                                          *)
(* What is modelled step by step: the post of each op, the shard's apply   *)
(* of each op, the fire, the cell's flags, the waker slot (register, look   *)
(* again, take, wake), the poll's register-then-look-again, the drop, the  *)
(* first shard's going and the sleep's arming again. What is abstract:     *)
(* time is one flag that turns on when the deadline passes (or, for a      *)
(* dropped sleep, may never within the run); the waker slot is one cell    *)
(* with one task's waker; a new cell after an orphan reuses the old cell's *)
(* variables, the old one being gone. Its register-then-look-again         *)
(* handshake guards a waker that changed between polls, which one task     *)
(* cannot show: that handshake is checked by the loom models of            *)
(* nuclei::sync's waker cell.                                              *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    DropIt,    \* TRUE: the sleep is dropped (at some point); FALSE: it is awaited to the end
    Goes,      \* TRUE: the first shard may go before it fires the timer
    Mutation   \* "none", or the name of the one rule this run takes away

ForeignT == 1   \* the thread that runs no shard, polling the sleep
ShardT == 2     \* the first shard, the one the sleep asks first; it may go
ClockT == 3     \* time, which passes the deadline
OtherT == 4     \* the second shard, where an orphaned timer is armed again; it never goes
Shards == {ShardT, OtherT}

(* --algorithm Timer
variables
    mailbox = [s \in Shards |-> <<>>],     \* each shard's mailbox: ops in the order they were posted
    entry = [s \in Shards |-> "none"],     \* each wheel's entry: "none", "armed" or "fired" (not removed yet)
    keyStored = [s \in Shards |-> FALSE],  \* the cell knows the entry's key (written by that shard)
    armedOn = ShardT,   \* the shard the cell's arm request went to
    fired = FALSE,      \* the cell's `fired` flag
    cancelled = FALSE,  \* the cell's `cancelled` flag
    orphaned = FALSE,   \* the cell's `orphaned` flag: its shard let go of it unfired
    slot = FALSE,       \* the cell's waker slot holds the foreign task's waker
    passed = FALSE,     \* the deadline has passed
    woken = FALSE,      \* the foreign task's waker was woken
    alive = TRUE,       \* the sleep exists
    gone = FALSE,       \* the first shard went
    \* records
    wokeDead = FALSE,   \* a wake reached a task whose sleep was dropped
    rearmed = FALSE;    \* the sleep armed its timer again on the second shard

define
    AllDone == \A p \in {ForeignT, ClockT} \cup Shards : pc[p] = "Done"
    \* Whether the first shard holds a timer for the cell: armed in its wheel, or asked for.
    HeldByFirst == entry[ShardT] = "armed"
                   \/ \E i \in 1..Len(mailbox[ShardT]) : mailbox[ShardT][i] = "insert"
end define;

\* `RemoteTimer::orphan`: the cell's shard let go of it unfired. Unless it fired or was cancelled,
\* it is marked orphaned and its waker taken and woken.
macro Orphan() begin
    if ~fired /\ ~cancelled then
        orphaned := TRUE;
        if slot then
            slot := FALSE;
            woken := TRUE;
            if ~alive then wokeDead := TRUE; end if;
        end if;
    end if;
end macro;

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
    \* First poll: register the waker in the cell, then post the arm request to the first shard.
    \* One that went drains what is posted to it at once: the arm request orphans the cell.
    if gone /\ Mutation # "no_orphan" then
        \* The waker registered, then taken and woken by the drain.
        orphaned := TRUE || woken := TRUE;
    elsif gone then
        \* MUTATION no_orphan: the drain lets the request go and wakes nobody.
        slot := TRUE;
    else
        slot := TRUE || mailbox[ShardT] := Append(mailbox[ShardT], "insert");
    end if;
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
    \* `Sleep::poll`: an orphaned cell is let go and the timer armed again; else
    \* `poll_cell`: look at `fired`; if not set, register and look again.
    if fired then
        goto F_done;
    elsif orphaned then
        goto F_rearm;
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
F_rearm:
    \* MUTATION no_rearm: the sleep waits on the orphaned cell, which nothing will fire.
    if Mutation = "no_rearm" then
        slot := TRUE || woken := FALSE;
    else
        \* The old cell goes (`State::Idle`), and a new one is armed where the sleep is polled:
        \* its waker registered, its request posted to the second shard.
        orphaned := FALSE || armedOn := OtherT || slot := TRUE || woken := FALSE
            || rearmed := TRUE || mailbox[OtherT] := Append(mailbox[OtherT], "insert");
    end if;
    goto F_wait;
F_drop:
    \* `release`: mark cancelled, take the waker out, post the cancel (drained at once by a
    \* shard that went).
    cancelled := TRUE;
    \* MUTATION drop_keeps_waker: the drop leaves the waker in the cell.
    if Mutation # "drop_keeps_waker" then slot := FALSE; end if;
    alive := FALSE;
    if ~(armedOn = ShardT /\ gone) then
        mailbox[armedOn] := Append(mailbox[armedOn], "cancel");
    end if;
F_done:
    skip;
end process;

fair process shard \in Shards
begin
S_loop:
    \* The shard's look at its timers: apply the mailbox, then fire what is due. It stops
    \* once the sleep and the clock are done and nothing is left to do (the model's end).
    \* The first shard may go instead, at any look.
    either
        await self = ShardT /\ Goes /\ ~gone;
        goto S_go;
    or
        if mailbox[self] # <<>> then
            goto S_apply;
        elsif entry[self] = "armed" /\ passed then
            goto S_fire;
        elsif pc[ForeignT] = "Done" /\ pc[ClockT] = "Done" then
            goto Done;
        else
            await mailbox[self] # <<>> \/ (entry[self] = "armed" /\ passed)
                  \/ (pc[ForeignT] = "Done" /\ pc[ClockT] = "Done");
            goto S_loop;
        end if;
    end either;
S_apply:
    \* One op, in order.
    if Head(mailbox[self]) = "insert" then
        if ~cancelled then
            entry[self] := "armed";
            keyStored[self] := TRUE;
        end if;
    else
        \* "cancel": remove the entry if the cell knows it.
        \* MUTATION cancel_keeps_entry: the cancel does not remove it.
        if keyStored[self] /\ Mutation # "cancel_keeps_entry" then
            entry[self] := "none";
            keyStored[self] := FALSE;
        end if;
    end if;
    mailbox[self] := Tail(mailbox[self]);
    goto S_loop;
S_fire:
    \* The wheel fires the entry; the cell's wake removes it, sets `fired`, wakes the waker.
    \* MUTATION fire_keeps_entry: the fired entry stays in the wheel.
    if Mutation = "fire_keeps_entry" then
        entry[self] := "fired";
    else
        entry[self] := "none";
    end if;
    keyStored[self] := FALSE;
    fired := TRUE;
S_wake:
    if slot then
        slot := FALSE;
        woken := TRUE;
        if ~alive then wokeDead := TRUE; end if;
    end if;
    goto S_loop;
S_go:
    \* The first shard goes (its drop): its mailbox is drained and its wheel lets go of its
    \* entry; a timer it held for the cell, armed or asked for, orphans the cell.
    \* MUTATION no_orphan: it lets go of them and wakes nobody.
    if HeldByFirst /\ Mutation # "no_orphan" then
        Orphan();
    end if;
    gone := TRUE;
    entry[self] := "none";
    keyStored[self] := FALSE;
    mailbox[self] := <<>>;
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

\* Once everything is done, nothing of the timer is left in any wheel.
NothingLeft == AllDone => \A s \in Shards : entry[s] = "none"

\* No wake reaches the task of a sleep that was dropped.
NoWakeAfterDrop == ~wokeDead

\* Once the first shard went, no cell of a timer it held keeps the task's waker unwoken: the
\* task and the cell never keep each other for good.
NoStrandedWaker == ~(gone /\ armedOn = ShardT /\ slot /\ ~orphaned /\ ~fired /\ ~cancelled)

Safe == NothingLeft /\ NoWakeAfterDrop /\ NoStrandedWaker

(***************************************************************************)
(* Witnesses.                                                              *)
(***************************************************************************)

\* The sleep is dropped after its timer was armed.
NoCancelOfArmed == ~(cancelled /\ entry[ShardT] = "armed")
\* The sleep is dropped before the shard applied the arm request.
NoCancelBeforeArm == ~(cancelled /\ mailbox[ShardT] = <<"insert", "cancel">>)
\* The timer fires and wakes the waiting task.
NoFireWake == ~(fired /\ woken)
\* The first shard goes, and the sleep arms its timer again on the second.
NoRearm == ~rearmed
=============================================================================

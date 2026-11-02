------------------------------- MODULE Timer -------------------------------
(***************************************************************************)
(* A timer a shard fires for a thread that runs no shard                  *)
(* (src/time/sleep.rs: `RemoteTimer`, `Armed`, `TimerOp`, `Timer::poll`,   *)
(* `poll_any`, `poll_remote`, `poll_remote_cell`, `poll_cell`;             *)
(* src/rt/shard.rs: `Shared::post_timer`; src/rt/runtime.rs:               *)
(* `service_shard`; src/rt/shard/going.rs: the shard's drop), at the grain *)
(* of its atomic steps.                                                    *)
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
(* The sleep may move to another task between two polls (`Moves`): the    *)
(* cell then holds the first task's waker until the second task's poll     *)
(* registers its own. A poll looks at `fired`, registers, then looks at    *)
(* both marks: the orphan mark is looked at only after the register, since *)
(* a shard that went before it woke the waker the cell held then, the      *)
(* first task's, and nothing would wake the second.                        *)
(*                                                                          *)
(* The first shard may be one nobody drives (`Driven` FALSE): the own      *)
(* shard of a thread outside `block_on`, which `service_shard` falls back  *)
(* to when no worker takes work. A witness of an open question, not fixed: *)
(* its mailbox is never applied, and the sleep never ends.                 *)
(*                                                                          *)
(* What is modelled step by step: the post of each op, the shard's apply   *)
(* of each op, the fire, the cell's flags, the waker slot (register, look   *)
(* again, take, wake), the poll's looks and register, the drop, the move, *)
(* the first shard's going and the sleep's arming again. What is abstract: *)
(* time is one flag that turns on when the deadline passes (or, for a      *)
(* dropped sleep, may never within the run); the waker slot names which of *)
(* two tasks' wakers it holds; a task that is woken is polled again, and a *)
(* wake that comes during its poll is kept for its next one; a new cell    *)
(* after an orphan reuses the old cell's variables, the old one being      *)
(* gone. The register-then-look-again handshake of the waker slot itself   *)
(* is checked by the loom models of nuclei::sync's waker cell.             *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    DropIt,    \* TRUE: the sleep is dropped (at some point); FALSE: it is awaited to the end
    Goes,      \* TRUE: the first shard may go before it fires the timer
    Moves,     \* TRUE: the sleep may move to a second task once, between two polls
    Driven,    \* TRUE: the first shard runs its loop; FALSE: no thread drives it
    Mutation   \* "none", or the name of the one rule this run takes away

ForeignT == 1   \* the thread that runs no shard, polling the sleep
ShardT == 2     \* the first shard, the one the sleep asks first; it may go
ClockT == 3     \* time, which passes the deadline
OtherT == 4     \* the second shard, where an orphaned timer is armed again; it never goes
Shards == {ShardT, OtherT}
\* The two tasks that may hold the sleep, by their wakers, and "none" for an empty slot.
Wakers == {"w1", "w2"}

(* --algorithm Timer
variables
    mailbox = [s \in Shards |-> <<>>],     \* each shard's mailbox: ops in the order they were posted
    entry = [s \in Shards |-> "none"],     \* each wheel's entry: "none", "armed" or "fired" (not removed yet)
    keyStored = [s \in Shards |-> FALSE],  \* the cell knows the entry's key (written by that shard)
    armedOn = ShardT,   \* the shard the cell's arm request went to
    fired = FALSE,      \* the cell's `fired` flag
    cancelled = FALSE,  \* the cell's `cancelled` flag
    orphaned = FALSE,   \* the cell's `orphaned` flag: its shard let go of it unfired
    slot = "none",      \* the waker the cell's slot holds: a task's, or none
    holder = "w1",      \* the task that holds the sleep now, by its waker
    passed = FALSE,     \* the deadline has passed
    woken = [w \in Wakers |-> FALSE], \* the task was woken and not polled since
    alive = TRUE,       \* the sleep exists
    gone = FALSE,       \* the first shard went
    \* records
    wokeDead = FALSE,   \* a wake reached a task whose sleep was dropped
    rearmed = FALSE,    \* the sleep armed its timer again on the second shard
    orphanFired = FALSE; \* (record) an orphan met a cell that had fired

define
    AllDone == \A p \in {ForeignT, ClockT} \cup Shards : pc[p] = "Done"
    \* Whether the first shard holds a timer for the cell: armed in its wheel, or asked for.
    HeldByFirst == entry[ShardT] = "armed"
                   \/ \E i \in 1..Len(mailbox[ShardT]) : mailbox[ShardT][i] = "insert"
end define;

\* Wakes the waker the cell's slot holds, taking it out of the slot: the task it belongs to is
\* polled again. A task woken after its sleep was dropped is recorded.
macro WakeSlot() begin
    if slot # "none" then
        woken[slot] := TRUE;
        if ~alive then wokeDead := TRUE; end if;
        slot := "none";
    end if;
end macro;

\* `RemoteTimer::orphan`: the cell's shard let go of it unfired. Unless it fired or was cancelled,
\* it is marked orphaned and its waker taken and woken.
macro Orphan() begin
    orphanFired := orphanFired \/ fired;
    \* MUTATION orphan_ignores_cancel: a cancelled cell is orphaned and woken too.
    if ~fired /\ (~cancelled \/ Mutation = "orphan_ignores_cancel") then
        orphaned := TRUE;
        if slot # "none" then
            woken[slot] := TRUE;
            \* (record) a wake once the drop began (its cancel is its first step)
            if ~alive \/ cancelled then wokeDead := TRUE; end if;
            slot := "none";
        end if;
    end if;
end macro;

\* `Shared::drain_timer_ops`: every op in the first shard's mailbox goes; an arm request
\* orphans its cell.
macro DrainFirst() begin
    \* MUTATION no_orphan: the drain lets the request go and wakes nobody.
    if (\E i \in 1..Len(mailbox[ShardT]) : mailbox[ShardT][i] = "insert")
       /\ Mutation # "no_orphan" then
        Orphan();
    end if;
    mailbox[ShardT] := <<>>;
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
variables sawGone = FALSE;    \* what the mutated post read of the flag before its push
begin
F_poll:
    \* First poll: register the waker in the cell, then `Shared::post_timer`: the arm request
    \* pushed to the first shard's mailbox, a fence, then the look at its `stopped` flag.
    \* MUTATION post_check_first: the flag is read before the push (what the fence stops).
    slot := holder;
    if Mutation = "post_check_first" then
        sawGone := gone;
    else
        mailbox[ShardT] := Append(mailbox[ShardT], "insert");
    end if;
F_post_check:
    \* A shard that stopped applies nothing more: the poster drains what reached it.
    \* MUTATION post_no_stop_check: the poster never looks at the flag.
    if Mutation = "post_check_first" then
        \* The push comes now; a flag seen up before it drains it with the rest.
        if sawGone then
            if Mutation # "no_orphan" then Orphan(); end if;
            mailbox[ShardT] := <<>>;
        else
            mailbox[ShardT] := Append(mailbox[ShardT], "insert");
        end if;
    elsif gone /\ Mutation # "post_no_stop_check" then
        DrainFirst();
    end if;
F_wait:
    \* Await the fire (a wake of the task that holds the sleep), move the sleep to the second
    \* task, whose poll comes at once, or drop the sleep.
    either
        await DropIt;
        goto F_drop;
    or
        await woken[holder] \/ (DropIt = FALSE /\ fired);
    or
        await Moves /\ holder = "w1";
        holder := "w2";
    end either;
F_look:
    \* A poll: it consumes the task's wake, and `poll_cell` looks at `fired`.
    woken[holder] := FALSE;
    if fired then
        goto F_done;
    end if;
F_register:
    \* Register the poll's waker: the slot holds the task's waker from now on.
    slot := holder;
F_recheck:
    \* Look again after registering, at both marks: a fire or an orphaning before the register
    \* woke the waker the slot held then, which may be the other task's. An orphaned cell is let
    \* go and the timer armed again where the poll runs.
    \* MUTATION recheck_fired_only: the look after the register is at `fired` alone.
    if fired then
        goto F_done;
    elsif orphaned /\ Mutation # "recheck_fired_only" then
        goto F_rearm;
    else
        \* Pending: polled again once woken, or dropped meanwhile, as at `F_wait`.
        either
            await woken[holder];
            goto F_look;
        or
            await DropIt;
            goto F_drop;
        end either;
    end if;
F_rearm:
    \* MUTATION no_rearm: the sleep waits on the orphaned cell, which nothing will fire.
    if Mutation = "no_rearm" then
        slot := holder;
    else
        \* The old cell goes (`State::Idle`), and a new one is armed where the sleep is polled:
        \* its waker registered, its request posted to the second shard.
        orphaned := FALSE || armedOn := OtherT || slot := holder
            || rearmed := TRUE || mailbox[OtherT] := Append(mailbox[OtherT], "insert");
    end if;
    goto F_wait;
F_drop:
    \* `release_armed`: mark cancelled, ...
    cancelled := TRUE;
F_drop_waker:
    \* ... take the waker out ...
    \* MUTATION drop_keeps_waker: the drop leaves the waker in the cell.
    if Mutation # "drop_keeps_waker" then slot := "none"; end if;
    alive := FALSE;
F_drop_post:
    \* ... and post the cancel (drained at once by a shard that went: a cancel does nothing).
    if ~(armedOn = ShardT /\ gone) then
        mailbox[armedOn] := Append(mailbox[armedOn], "cancel");
    elsif Mutation # "post_no_stop_check" then
        DrainFirst();
    end if;
F_done:
    skip;
end process;

fair process shard \in Shards
begin
S_loop:
    \* The shard's look at its timers: apply the mailbox, then fire what is due. It stops
    \* once the sleep and the clock are done and nothing is left to do (the model's end).
    \* The first shard may go instead, at any look. A first shard nobody drives never looks.
    either
        await self = ShardT /\ Goes /\ ~gone;
        goto S_go;
    or
        await self # ShardT \/ Driven;
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
    \* The wheel fires the entry and lets go of it as it fires (`Wheel::advance`); the cell's
    \* wake sets `fired` and wakes the waker.
    \* MUTATION fire_keeps_entry: the fired entry stays in the wheel.
    if Mutation = "fire_keeps_entry" then
        entry[self] := "fired";
    else
        entry[self] := "none";
    end if;
    keyStored[self] := FALSE;
    fired := TRUE;
S_wake:
    WakeSlot();
    goto S_loop;
S_go:
    \* The first shard goes (its drop): its `stopped` flag goes up first, then a fence ...
    gone := TRUE;
S_go_wheel:
    \* ... its wheel lets go of its entry: a timer it held armed orphans the cell ...
    \* MUTATION no_orphan: it lets go of what it held and wakes nobody.
    if entry[self] = "armed" /\ Mutation # "no_orphan" then
        Orphan();
    end if;
    entry[self] := "none";
    keyStored[self] := FALSE;
S_go_drain:
    \* ... and its last drain of the mailbox: an arm request there orphans the cell too.
    DrainFirst();
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
\* (Checked once nothing is about to drain: not between a post's push and its look at the
\* flag, nor while the going shard is letting go of its wheel and draining its mailbox.)
NoStrandedWaker ==
    (pc[ForeignT] # "F_post_check" /\ pc[ShardT] \notin {"S_go_wheel", "S_go_drain"})
        => ~(gone /\ armedOn = ShardT /\ slot # "none" /\ ~orphaned /\ ~fired /\ ~cancelled)

\* An orphan never meets a cell that fired: the fired half of `orphan`'s check never decides
\* anything (the shard that fires a cell is the one that would orphan it).
NoOrphanAfterFire == ~orphanFired

\* Once every thread is done, no arm request is left in a mailbox nobody will drain.
NoStrandedOp == AllDone => \A s \in Shards : mailbox[s] = <<>>

Safe == NothingLeft /\ NoWakeAfterDrop /\ NoStrandedWaker

(***************************************************************************)
(* Witnesses.                                                              *)
(***************************************************************************)

\* The sleep is dropped after its timer was armed.
NoCancelOfArmed == ~(cancelled /\ entry[ShardT] = "armed")
\* The sleep is dropped before the shard applied the arm request.
NoCancelBeforeArm == ~(cancelled /\ mailbox[ShardT] = <<"insert", "cancel">>)
\* The timer fires and wakes the waiting task.
NoFireWake == ~(fired /\ \E w \in Wakers : woken[w])
\* The first shard goes, and the sleep arms its timer again on the second.
NoRearm == ~rearmed

(***************************************************************************)
(* Liveness.                                                               *)
(***************************************************************************)

\* The awaited sleep ends: the thread that polls it gets to its end. (`Termination`, which the
\* translation defines, asks every thread to end, a shard that nobody drives too.)
SleepEnds == <>(pc[ForeignT] = "Done")
=============================================================================

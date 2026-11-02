------------------------------ MODULE TimerLocal ------------------------------
(***************************************************************************)
(* A sleep armed in its own shard's wheel (src/time/sleep.rs:              *)
(* `Timer::poll_local`, `Timer::release_armed`, `TimerOp::Remove`;         *)
(* src/rt/shard/going.rs: the going shard's `take_wakers`), when its task  *)
(* moves to another shard or the shard goes.                               *)
(*                                                                          *)
(* In plain words: polled on a shard, a sleep arms a timer in that shard's *)
(* wheel and keeps its key; polled there again it is done once the timer   *)
(* fired, else it sets the poll's waker on the entry. Polled on another    *)
(* shard (its task moved there), it lets go of the first arming: on the    *)
(* owner's thread it removes the entry itself, from anywhere else it posts *)
(* a removal to the owner's mailbox (drained by the poster when the owner  *)
(* stopped); then it arms on the shard it is polled on. Dropped, it lets go *)
(* the same way. A shard that goes takes the waker of every timer it holds *)
(* and wakes it, so a sleep whose task moved arms again where it runs.     *)
(*                                                                          *)
(* Who acts: the sleep's task (on the first shard, then maybe moved to the *)
(* second), the first shard (it applies its mailbox, fires due timers, and *)
(* may go), the second shard (it never goes), and the clock. What must     *)
(* never happen: an entry left in a wheel for a sleep that let go of it;   *)
(* an awaited sleep whose task is never woken.                             *)
(*                                                                          *)
(* What is abstract: a wheel holds this one sleep's entry or none; time is *)
(* one flag; the task's waker is one flag on the entry (it wakes the task  *)
(* wherever it runs).                                                      *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    DropIt,    \* the sleep is dropped (from the second shard) instead of awaited
    Goes,      \* the first shard may go
    Mutation   \* "none", or the name of the one rule this run takes away

Shards == {"a", "b"}

(* --algorithm TimerLocal
variables
    entry = [s \in Shards |-> FALSE],        \* the shard's wheel holds the sleep's entry
    entryWaker = [s \in Shards |-> FALSE],   \* the entry holds the task's waker
    mailbox = <<>>,                          \* removals posted to the first shard
    armedOn = "none",                        \* the sleep's state: Local on that shard, or none
    done = FALSE,                            \* the sleep completed
    passed = FALSE,                          \* the deadline passed
    moved = FALSE,                           \* the task runs on the second shard now
    woken = FALSE,                           \* the task's current waker was called
    gone = FALSE,                            \* the first shard went
    released = FALSE;                        \* the sleep let go (dropped, or it completed)

define
    \* The shard the task runs on now.
    Here == IF moved THEN "b" ELSE "a"
    \* Once all is done, no wheel holds an entry for the sleep.
    NothingLeft ==
        (\A p \in {"task", "clock"} \cup Shards : pc[p] = "Done") => \A s \in Shards : ~entry[s]
    \* An awaited sleep completes.
    Completes == ~DropIt => <>done
end define;

fair process clock = "clock"
begin
C_pass:
    \* An awaited sleep's deadline passes; a dropped one's may be far off and never come
    \* within the run.
    if DropIt then
        either passed := TRUE; or skip; end either;
    else
        passed := TRUE;
    end if;
end process;

fair process task = "task"
begin
T_first:
    \* The first poll, on the first shard: armed there.
    entry["a"] := TRUE;
    entryWaker["a"] := TRUE;
    armedOn := "a";
T_run:
    \* The task may move to the second shard at any moment (a handoff); it polls when woken.
    either
        await ~moved;
        moved := TRUE;
        goto T_run;
    or
        await DropIt /\ moved;
        goto T_drop;
    or
        await woken /\ ~DropIt;
        woken := FALSE;
        goto T_poll;
    end either;
T_poll:
    \* `poll_local` on the shard the task runs on.
    if armedOn = Here then
        if ~entry[Here] then
            \* Fired: its entry went as it fired.
            done := TRUE;
            released := TRUE;
            armedOn := "none";
            goto Done;
        else
            \* Still armed: the poll's waker is set on the entry (`set_waker`).
            entryWaker[Here] := TRUE;
            goto T_run;
        end if;
    end if;
T_release:
    \* Armed elsewhere: let go of it first (`release_armed`), from this other thread a
    \* removal posted (or drained at once by a shard that stopped).
    \* MUTATION remove_not_posted: no removal is posted.
    if armedOn = "a" /\ Mutation # "remove_not_posted" then
        if gone then
            skip;                          \* drained by the poster: the going shard keeps nothing
        else
            mailbox := Append(mailbox, "remove");
        end if;
    end if;
    armedOn := "none";
T_rearm:
    \* Then armed where it is polled: due already, it is done.
    if passed then
        done := TRUE;
        released := TRUE;
        goto Done;
    else
        entry[Here] := TRUE;
        entryWaker[Here] := TRUE;
        armedOn := Here;
        goto T_run;
    end if;
T_drop:
    \* The drop on the second shard: the arming on the first let go as above.
    if armedOn = "a" /\ Mutation # "remove_not_posted" /\ ~gone then
        mailbox := Append(mailbox, "remove");
    elsif armedOn = "b" then
        entry["b"] := FALSE;               \* removed on its own shard
    end if;
    armedOn := "none";
    released := TRUE;
end process;

fair process shard \in Shards
begin
S_loop:
    \* Apply the mailbox, fire a due entry (waking its waker), or (the first) go.
    either
        await self = "a" /\ mailbox # <<>> /\ ~gone;
        entry["a"] := FALSE;               \* `remove`: the key's entry goes
        mailbox := Tail(mailbox);
    or
        await entry[self] /\ passed /\ ~(self = "a" /\ gone);
        entry[self] := FALSE;              \* fired: the entry goes as it fires
        woken := woken \/ entryWaker[self];
        entryWaker[self] := FALSE;
    or
        \* The first shard goes: every waker it holds taken and woken; the entry stays for
        \* its key, and its mailbox is drained.
        \* MUTATION going_keeps_wakers: the wakers stay where they are.
        \* (Only once the task moved: a task bound to the shard goes with it.)
        await self = "a" /\ Goes /\ ~gone /\ moved;
        gone := TRUE;
        if Mutation # "going_keeps_wakers" then
            woken := woken \/ (entry["a"] /\ entryWaker["a"]);
        end if;
        entryWaker["a"] := FALSE;
        entry["a"] := FALSE;               \* the going shard's wheel goes with it
        mailbox := <<>>;
    or
        await pc["task"] = "Done" /\ pc["clock"] = "Done"
              /\ (self = "b" \/ mailbox = <<>> \/ gone);
        goto Done;
    end either;
S_again:
    goto S_loop;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

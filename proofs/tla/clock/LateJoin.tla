------------------------------- MODULE LateJoin -------------------------------
(***************************************************************************)
(* A paused clock against a thread inside its runtime's `block_on` that    *)
(* entered before the pause (src/rt/runtime.rs: `block_on_here`;           *)
(* src/rt/paused.rs: `Guest::enter`, `Guest`'s drop, `pause`,              *)
(* `join_visitors` and `attempt`; the own shard's `visiting` mark in        *)
(* src/rt/shard.rs).                                                       *)
(*                                                                          *)
(* In plain words: a thread entering the runtime's `block_on` on its own   *)
(* shard marks the shard as visiting the runtime, then looks at the clock: *)
(* paused, it joins the clock's guests at once. A task on a worker pauses  *)
(* the clock and looks at every own shard marked as visiting the runtime,  *)
(* joining each one (a join that finds the mark gone after it joined takes *)
(* it back), all while it holds the clock's advance claim, so no jump is   *)
(* decided before the shards it finds are counted. The thread, inside, runs (busy) or parks   *)
(* with a timer armed (idle); leaving, it clears its mark and then leaves  *)
(* the guests. A jump attempt moves the paused clock to the earliest       *)
(* timer only when every member is idle, and looks twice.                  *)
(*                                                                          *)
(* Who acts: the pausing task, the visiting thread, and an attempt. What   *)
(* must never happen: the clock jumps while a thread inside the runtime's *)
(* `block_on` is busy, or past the timer it armed; a thread that left is   *)
(* still counted (it would hold every jump for good).                      *)
(*                                                                          *)
(* What is abstract: the workers are one idle shard with a timer at Far;   *)
(* the visiting thread parks once with a timer Near units on; the member   *)
(* count and the second look are as in ClockJoin.tla, one step each.      *)
(*                                                                          *)
(* Not built: the mark this design reads would cost instructions on every  *)
(* outermost `block_on`, which 0.5.4 may not add. The code today is the    *)
(* mutation `M_no_late_join`; the GREEN configurations check the design.  *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Near,      \* how far from the clock the visiting thread arms its timer
    Far,       \* the workers' timer
    Leaves,    \* the visiting thread leaves `block_on` after it parked
    Mutation   \* "none", or the name of the one rule this run takes away

\* No timer: past every time the model reaches.
Inf == 1000
\* The workers' timer, once: it fired when the clock reached it.
WorkersAt(c) == IF c < Far THEN Far ELSE Inf

(* --algorithm LateJoin
variables
    clock = 0,              \* the clock
    paused = FALSE,         \* the clock is paused
    visiting = FALSE,       \* the own shard's mark: its thread is inside the runtime's block_on
    guest = FALSE,          \* the own shard is among the clock's guests
    members = 0,            \* the member count, bumped by every join and leave
    busy = TRUE,            \* the visiting thread runs (not parked)
    inside = FALSE,         \* the visiting thread runs its future inside block_on
    armed = FALSE,          \* its timer is armed
    at = 0,                 \* its timer
    advancing = FALSE,      \* the clock's advance claim (`claim_advance`) is held
    badJump = FALSE;        \* (record) the clock jumped while the thread inside was busy

define
    \* The clock never jumps while a thread inside the runtime's block_on is busy.
    NoBadJump == ~badJump
    \* No timer the visiting thread armed is behind the clock.
    ClockNeverPassesArmed == armed => clock <= at
    \* A shard is a guest only while its thread is inside (eventually, once it left).
    NoStaleGuest == <>[](~inside => ~guest)
    \* The visiting thread's sleep comes due and it ends.
    ThreadEnds == <>(pc["thread"] = "Done")
end define;

fair process thread = "thread"
begin
T_mark:
    \* `block_on_here`: the mark first, then the look at the clock.
    \* MUTATION no_visit_mark: no mark, so a pause from elsewhere cannot find the thread.
    \* MUTATION join_at_look: no mark either; the thread joins a paused clock only at its own next
    \* timer look (before it parks), the shape that costs `block_on` nothing.
    visiting := Mutation \notin {"no_visit_mark", "join_at_look"};
T_look:
    \* From its look on it runs the future: inside.
    inside := TRUE;
    if paused then
        guest := TRUE;
        members := members + 1;
    end if;
T_arm:
    \* It runs, arms its timer Near units from the clock, and parks; or, with Leaves, its future
    \* may end at once (woken by something else) and the thread leaves.
    either
        at := clock + Near;
        armed := TRUE;
        if Mutation = "join_at_look" then
            goto T_timer_look;
        else
            busy := FALSE;
        end if;
    or
        await Leaves;
        goto T_leave;
    end either;
T_timer_look:
    \* (join_at_look) The timer look before the park: a paused clock it is not a member of is
    \* joined here, before the deadline is read; then it parks.
    if Mutation = "join_at_look" then
        if paused /\ ~guest then
            guest := TRUE;
            members := members + 1;
        end if;
        busy := FALSE;
    end if;
T_park:
    \* Parked until its timer comes due on the clock; then it runs.
    await clock >= at;
    armed := FALSE;
    busy := TRUE;
T_leave:
    \* Leaving `block_on`: the mark goes, then the guest (Guest's drop).
    if Leaves then
        visiting := FALSE;              \* its future is over: no longer inside
        inside := FALSE;
T_unguest:
        if guest then
            guest := FALSE;
            members := members + 1;
        end if;
    end if;
end process;

fair process pauser = "pauser"
variables saw = FALSE;
begin
P_claim:
    \* `time::pause()` from a task on a worker claims the advance first.
    \* MUTATION pause_unclaimed: it does not, and a jump may be decided before it joined anybody.
    if Mutation # "pause_unclaimed" then
        await ~advancing;
        advancing := TRUE;
    end if;
P_pause:
    \* The clock stops.
    paused := TRUE;
P_look:
    \* `join_visitors`: it looks at each own shard's mark.
    \* MUTATION no_late_join: it looks at none (only the calling thread's own shard).
    saw := visiting /\ Mutation # "no_late_join";
P_join:
    \* Each own shard it saw marked joins the guests.
    if saw then
        if ~guest then
            guest := TRUE;
            members := members + 1;
        end if;
P_recheck:
        \* The join looks at the mark again: gone (the thread left meanwhile), it takes the join
        \* back, as the leaving thread may have looked before the join landed.
        \* MUTATION no_join_recheck: it does not look again.
        if ~visiting /\ guest /\ Mutation # "no_join_recheck" then
            guest := FALSE;
            members := members + 1;
        end if;
    end if;
P_release:
    \* The advance claim goes.
    if Mutation # "pause_unclaimed" then
        advancing := FALSE;
    end if;
end process;

fair process attempt = "attempt"
variables seen = 0, earliest = Inf;
begin
A_members:
    \* `try_advance`, which every park runs again: once the clock is paused, the advance
    \* claimed, the member count read. It stops once the thread ended.
    if pc["thread"] = "Done" then
        goto Done;
    end if;
A_claim:
    await paused /\ ~advancing;
    advancing := TRUE;
    seen := members;
A_look:
    \* Every member idle (the guest, if one, parked); the earliest timer noted.
    if guest /\ busy then
        advancing := FALSE;
        goto A_members;
    else
        earliest := IF guest /\ armed /\ at < WorkersAt(clock) THEN at ELSE WorkersAt(clock);
    end if;
A_jump:
    \* The second look: the count unchanged and the guest still idle; then the jump.
    if members = seen /\ ~(guest /\ busy) /\ earliest < Inf then
        badJump := inside /\ busy;
        clock := IF earliest > clock THEN earliest ELSE clock;
    end if;
    advancing := FALSE;
    goto A_members;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

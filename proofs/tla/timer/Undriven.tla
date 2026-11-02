------------------------------- MODULE Undriven -------------------------------
(***************************************************************************)
(* A sleep polled on a thread that runs no shard while no worker of its    *)
(* runtime takes tasks (src/rt/runtime.rs: `service_shard`; src/time/      *)
(* sleep.rs: `Timer::poll_remote`; src/rt/shard.rs: `Shared::post_timer`;  *)
(* the own shard's `visiting` mark; src/rt/shard/going.rs: `Shard::leave`, *)
(* which orphans the remote timers a leaving own shard holds).             *)
(*                                                                          *)
(* In plain words: such a sleep posts its arm request to a shard the       *)
(* runtime names. With no worker to take it, that is an own shard: the     *)
(* calling thread's, which nobody drives while that thread is outside      *)
(* `block_on`, or the own shard of a thread inside the runtime's           *)
(* `block_on`, whose loop runs: the post wakes that thread, its loop       *)
(* applies the request, and it fires the sleep when the deadline passes.   *)
(* A thread that leaves `block_on` lets go of the timers it holds for      *)
(* others: each one's cell is orphaned and its waker woken, and the sleep, *)
(* polled again, asks again.                                               *)
(*                                                                          *)
(* Who acts: the polling thread, the visiting thread, time. What must      *)
(* never happen: the sleep never ending.                                   *)
(*                                                                          *)
(* What is abstract: time is one flag; the cell is `fired`, `orphaned` and *)
(* the target's mailbox; one sleep, one visiting thread that may leave     *)
(* once.                                                                   *)
(*                                                                          *)
(* Not built: the mark this design reads would cost instructions on every  *)
(* outermost `block_on`, which 0.5.4 may not add. The code today is the    *)
(* mutation `M_own_fallback`; the GREEN configurations check the design.  *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Leaves,    \* the visiting thread leaves `block_on` (once) while it may hold the timer
    Mutation   \* "none", or the name of the one rule this run takes away

Own == "own"          \* the polling thread's own shard: nobody drives it
Visitor == "visitor"  \* the own shard of a thread inside the runtime's block_on
None == "none"

(* --algorithm Undriven
variables
    inside = TRUE,      \* the visiting thread is inside the runtime's block_on
    passed = FALSE,     \* the sleep's deadline passed
    target = None,      \* the shard the arm request went to
    posted = FALSE,     \* the request waits in the target's mailbox
    armedThere = FALSE, \* the target armed it in its wheel
    fired = FALSE,      \* the cell's fired mark
    orphaned = FALSE,   \* the cell's orphaned mark
    woken = FALSE;      \* the poller's waker was called

define
    \* The sleep ends.
    SleepEnds == <>(pc["poller"] = "Done")
end define;

fair process time = "time"
begin
C_pass:
    passed := TRUE;
end process;

fair process poller = "poller"
begin
P_arm:
    \* `poll_remote`: the shard to post to (`service_shard`): with no worker, a visiting own
    \* shard if one is inside, else the calling thread's own. MUTATION own_fallback: the calling
    \* thread's own shard always (the code before 0.5.4's fix).
    target := IF inside /\ Mutation # "own_fallback" THEN Visitor ELSE Own;
    posted := TRUE;
    orphaned := FALSE;
P_wait:
    \* Asleep until its waker is called: fired, it ends; orphaned, it asks again.
    await woken;
    woken := FALSE;
    if fired then
        goto Done;
    elsif orphaned then
        goto P_arm;
    else
        goto P_wait;
    end if;
end process;

fair process visitor = "visitorT"
begin
V_loop:
    \* Its loop, while its thread is inside: apply the mailbox, fire what is due; or leave.
    while inside do
        either
            await target = Visitor /\ posted;
            posted := FALSE;
            armedThere := TRUE;
        or
            await target = Visitor /\ armedThere /\ passed /\ ~fired;
            fired := TRUE;
            armedThere := FALSE;
            woken := TRUE;
        or
            await Leaves;
            inside := FALSE;
V_leave:
            \* `Shard::leave`: every remote timer it holds, armed or asked for, is orphaned and
            \* its waker woken. MUTATION leave_keeps_timers: they stay, with nobody to fire them.
            if target = Visitor /\ (posted \/ armedThere) /\ ~fired
               /\ Mutation # "leave_keeps_timers" then
                posted := FALSE;
                armedThere := FALSE;
                orphaned := TRUE;
                woken := TRUE;
            end if;
        end either;
    end while;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

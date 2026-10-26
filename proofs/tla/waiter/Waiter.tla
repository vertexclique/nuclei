------------------------------- MODULE Waiter -------------------------------
(***************************************************************************)
(* A waiter node (src/sync/waiter/node.rs, wait.rs, wake.rs, pool.rs), at  *)
(* the grain of its state word's compare-and-swaps and of each access to  *)
(* its two waker slots.                                                    *)
(*                                                                          *)
(* In plain words: a future takes a node from its primitive's pool, arms  *)
(* it with its waker and queues it. A drain pass nudges it (wakes it to    *)
(* try again) or grants it, claiming the armed waker for the pass's wake  *)
(* list if nobody is registering or taking; otherwise it only sets its     *)
(* mark. The wake list takes the waker from the taken slot, claims again  *)
(* a waker armed meanwhile on a granted or nudged node, and calls them.   *)
(* The future registers a new waker in the slot a take is not reading,    *)
(* and reads the marks in the swap that arms it. A spinning future first  *)
(* disarms its waker, so a mark during the spin claims nothing. A future   *)
(* that leaves withdraws: it cancels the node and takes back its armed     *)
(* waker. The queue, the future and a wake list each hold a reference     *)
(* bit; whoever clears the last one returns the node to the pool, where   *)
(* the next future takes it. A future past the bypass bound marks itself  *)
(* owed; the mark is taken back while it waits, or settled as the queue   *)
(* lets the node go.                                                       *)
(*                                                                          *)
(* What is modelled step by step: every compare-and-swap of the state word *)
(* and every slot access, the drain passes in turn (the drain role is one  *)
(* at a time), each pass's wake list beside the later passes, the release *)
(* of each reference and the pool's reuse of the node. What is abstract:  *)
(* the queue's lists and the drain role (the families of each primitive   *)
(* check them); a waker is an id, and each registration brings a new one, *)
(* so the waker the future waits on is always its latest.                 *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Rounds,    \* how many futures use the node in turn, through the pool
    Nudges,    \* the nudges a node gets before its grant, in each round
    Spin,      \* a future may spin before it parks
    Cancel,    \* a future may leave (be dropped) instead of waiting
    Owe,       \* a future may mark itself owed while it waits
    Spurious,  \* how many times a future may be polled without its waker called
    Mutation   \* "none", or the name of the one rule this run takes away

Futures == 1..Rounds
MaxPasses == Rounds * (Nudges + 2)
Passes == 1..MaxPasses
WakeLists == {100 + i : i \in Passes}
Drainer == 50
Waker(f, k) == f * 10 + k

(* --algorithm Waiter
variables
    \* The node's state word.
    phase = "W", reg = FALSE, armed = FALSE, taking = FALSE,
    cur = 0, taken = 0, nudged = FALSE, owed = FALSE, refs = {},
    slot = [i \in 0..1 |-> 0],
    \* The pool and the queue.
    freed = TRUE,               \* the node is in the pool
    round = 0,                  \* the future that took it last
    queued = FALSE,             \* the queue holds the node
    owedCount = 0,              \* the queue's count of owed waiters
    \* The wakes.
    called = {},                \* the wakers called so far
    latest = [f \in Futures |-> 0],
    started = [i \in Passes |-> FALSE],
    claimedIn = [i \in Passes |-> FALSE],
    wakeHolder = 0,             \* the pass whose wake list holds the wake reference
    \* Records.
    againSeen = FALSE, spinMarked = FALSE, claimedWhileSpinning = FALSE,
    forgiven = FALSE, cancelTook = FALSE;

define
    Procs == Futures \cup {Drainer} \cup WakeLists
    AllDone == \A q \in Procs : pc[q] = "Done"
    AllEnd == <>AllDone
    \* No party acts on the node after it let go of its reference.
    FutureLabels == {"F_settle", "F_poll", "F_spin", "F_park1", "F_write", "F_arm2",
                     "F_owe", "F_cancel", "F_withdraw", "F_cancelRead", "F_releaseF"}
    NoStaleAccess ==
        /\ \A f \in Futures : pc[f] \in FutureLabels => "F" \in refs /\ round = f /\ ~freed
        /\ \A w \in WakeLists : pc[w] \in {"W_take", "W_cas"} =>
               "W" \in refs /\ wakeHolder = w - 100 /\ ~freed
        /\ pc[Drainer] \in {"D_nudge", "D_grant", "D_grantRel", "D_prune"} => "Q" \in refs /\ ~freed
    \* The waker is never armed while the future registers: every swap that
    \* sets the registering bit clears the armed one, so a claim, which needs
    \* an armed waker, never meets a registering future.
    ArmedApart == ~(armed /\ reg)
    \* A node in the pool is referenced by nobody.
    PoolFree == freed => refs = {}
    \* The owed count never goes below zero and settles to zero.
    OwedSettled == owedCount >= 0 /\ (AllDone => owedCount = 0)
    \* A mark that comes while the future spins claims no waker.
    NoClaimWhileSpinning == ~claimedWhileSpinning
    \* Witnesses.
    NoAgain == ~againSeen
    NoSpinMark == ~spinMarked
    NoReuse == round <= 1
    NoForgive == ~forgiven
    NoCancelTake == ~cancelTook
end define;

\* `touch`: sets `bit` on a waiting node, claiming its armed waker for pass
\* `p`'s wake list if nobody registers or takes; `once` leaves a node that
\* already carries a nudge alone.
macro touch(bit, once) begin
    if phase # "W" then
        touchRes := "skipped";
    elsif once /\ nudged then
        touchRes := "pending";
    elsif armed /\ ~taking /\ (~reg \/ Mutation = "claim_registering") then
        taken := cur;
        armed := FALSE;
        taking := TRUE;
        refs := refs \cup {"W"};
        wakeHolder := p;
        claimedIn[p] := TRUE;
        claimedWhileSpinning := claimedWhileSpinning \/ \E f \in Futures : pc[f] = "F_spin";
        if bit = "N" then nudged := TRUE; else phase := "G"; end if;
        touchRes := "claimed";
    else
        spinMarked := spinMarked \/ \E f \in Futures : pc[f] = "F_spin";
        if bit = "N" then nudged := TRUE; else phase := "G"; end if;
        touchRes := "marked";
    end if;
end macro;

\* `release`: one party's reference goes; the last party out puts the node
\* back in the pool. The queue's settles the owed mark.
macro release(bit) begin
    if bit = "Q" /\ owed /\ Mutation # "release_forgets_owed" then
        owedCount := owedCount - 1;
        forgiven := TRUE;
    end if;
    if refs = {bit} \/ (bit = "Q" /\ Mutation = "free_on_queue_release") then
        refs := {};
        freed := TRUE;
        slot := [i \in 0..1 |-> 0];
    else
        refs := refs \ {bit};
    end if;
end macro;

fair process fut \in Futures
variables k = 0, res = "pending", wasArmed = FALSE, readSlot = 0, owing = FALSE, sp = 0;
begin
F_take:
    \* `enqueue`: the node from the pool, armed with the first waker, queued.
    await round = self - 1 /\ freed;
    round := self;
    freed := FALSE;
    queued := TRUE;
    phase := "W"; reg := TRUE; armed := FALSE; taking := FALSE; cur := 0; taken := 0;
    nudged := FALSE; owed := FALSE; refs := {"Q", "F"};
    k := 1;
    slot[0] := Waker(self, 1);
    latest[self] := Waker(self, 1);
F_settle:
    \* `park(None)`: arms the waker `arm` wrote, reading the marks.
    res := IF phase = "G" THEN "granted" ELSE IF nudged THEN "nudged" ELSE "pending";
    reg := FALSE;
    nudged := FALSE;
    armed := TRUE;
F_next:
    if res = "granted" then
        goto F_releaseF;
    elsif res = "nudged" then
        \* It competes again: it loses and waits again, or wins and leaves.
        either
            goto F_poll;
        or
            await Cancel;
            goto F_cancel;
        end either;
    end if;
F_owe:
    \* `owe`: past the bypass bound, while it waits.
    if Owe /\ ~owing /\ phase = "W" then
        owed := TRUE;
        owedCount := owedCount + 1;
        owing := TRUE;
    end if;
F_wait:
    \* Parked: its latest waker wakes it, something else polls it (a
    \* combinator, say), or it is dropped.
    either
        await latest[self] \in called;
        goto F_poll;
    or
        await sp < Spurious;
        sp := sp + 1;
        goto F_poll;
    or
        await Cancel;
        goto F_cancel;
    end either;
F_poll:
    \* `begin_spin`: an armed waker is disarmed for the spin.
    if Spin then
        if phase = "G" \/ nudged then
            goto F_park1;
        elsif armed /\ Mutation # "spin_keeps_armed" then
            armed := FALSE;
        end if;
    else
        goto F_park1;
    end if;
F_spin:
    \* The spin's looks at the marks; the park below reads them in one swap.
    skip;
F_park1:
    \* `park(Some)`: a grant or a nudge is taken; else it registers, in the
    \* slot a take under way is not reading.
    if phase = "G" then
        res := "granted";
        goto F_next;
    elsif nudged then
        nudged := FALSE;
        res := "nudged";
        goto F_next;
    else
        cur := IF taking /\ Mutation # "park_ignores_taking" THEN 1 - taken ELSE cur;
        \* MUTATION claim_registering (with the claim's registering test):
        \* the waker stays armed while the future registers.
        armed := armed /\ Mutation = "claim_registering";
        reg := TRUE;
    end if;
F_write:
    k := k + 1;
    slot[cur] := Waker(self, k);
    latest[self] := Waker(self, k);
F_arm2:
    \* Arms the waker, reading the marks in the same swap.
    if Mutation = "arm_without_look" then
        res := "pending";
        reg := FALSE;
        armed := TRUE;
    else
        res := IF phase = "G" THEN "granted" ELSE IF nudged THEN "nudged" ELSE "pending";
        reg := FALSE;
        nudged := FALSE;
        armed := TRUE;
    end if;
    goto F_next;
F_cancel:
    \* `unowe`, while it still waits.
    if owing /\ phase = "W" /\ owed then
        owed := FALSE;
        owedCount := owedCount - 1;
    end if;
F_withdraw:
    \* `withdraw`: granted already, or cancelled, its armed waker taken back.
    if phase = "G" then
        res := "granted";
        goto F_releaseF;
    else
        wasArmed := armed;
        readSlot := cur;
        phase := "C";
        armed := FALSE;
    end if;
F_cancelRead:
    if wasArmed then
        slot[readSlot] := 0;
        cancelTook := TRUE;
    end if;
F_releaseF:
    release("F");
end process;

fair process drainer = Drainer
variables r = 1, n = 0, p = 0, touchRes = "none";
begin
D_round:
    while r <= Rounds do
D_wait:
        await round = r /\ pc[r] # "F_take";
        n := 0;
D_pass:
        while queued do
            p := p + 1;
            if phase = "C" then
D_prune:
                release("Q");
                queued := FALSE;
            elsif n < Nudges then
D_nudge:
                touch("N", TRUE);
                n := n + 1;
            else
D_grant:
                touch("G", FALSE);
D_grantRel:
                release("Q");
                queued := FALSE;
            end if;
D_end:
            \* The pass gives up the drain role; its wake list runs.
            started[p] := TRUE;
        end while;
        r := r + 1;
    end while;
end process;

fair process wakes \in WakeLists
variables wk = 0, again = FALSE;
begin
W_start:
    await started[self - 100] \/ pc[Drainer] = "Done";
    if ~claimedIn[self - 100] then
        goto Done;
    end if;
W_take:
    \* `take_claimed`: the waker of the taken slot.
    wk := slot[taken];
    slot[taken] := 0;
W_cas:
    if armed /\ ~reg /\ (nudged \/ phase = "G") /\ Mutation # "no_again" then
        \* A waker armed meanwhile on a granted or nudged node: claimed too.
        taken := cur;
        armed := FALSE;
        againSeen := TRUE;
        again := TRUE;
    else
        taking := FALSE;
        again := FALSE;
        release("W");
    end if;
W_call:
    if wk # 0 then
        called := called \cup {wk};
    end if;
    if again then
        goto W_take;
    end if;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

\* No two parties touch one slot at once: the slot a registering future
\* writes, and the one a withdrawing future reads, are never the one a take
\* under way reads.
SlotExclusive ==
    /\ \A f \in Futures : pc[f] = "F_write" => ~(taking /\ taken = cur)
    /\ \A f \in Futures : pc[f] = "F_cancelRead" /\ wasArmed[f] => ~(taking /\ taken = readSlot[f])
=============================================================================

------------------------------ MODULE Handoff ------------------------------
(***************************************************************************)
(* The shard handoff (src/rt/handoff/, its callers in src/rt/shard.rs and  *)
(* src/rt/context.rs: `poll_watched`, `run_task_watched`, the bracket of   *)
(* `bracketed`, which `current`, `current_held` and the wake and spawn a   *)
(* task's code makes on a watched shard go through, `queue_here_bracketed` *)
(* in src/rt/sched.rs, which first tries the leaf push of                  *)
(* `context::push_lifo_marked` (`Slot::mark_unless_claimed`), backing out  *)
(* to the full bracket; and the same mark kept by an IO owner's step,      *)
(* `mark_call` as `After::enter` makes it, ended by `unmark_call` as the   *)
(* step is settled, `After::settle_light_owned` and `Source::settle_rest`  *)
(* in src/io/) on a machine whose stores wait in a store buffer, as x86's  *)
(* do. A shard no monitor watches has no slot, and its polls               *)
(* (`poll_unwatched`) and calls take no part in this protocol.             *)
(*                                                                          *)
(* In plain words: one thread at a time runs a shard (its runner). Each    *)
(* runner has a slot of its own: a word it writes (in the loop, or in a    *)
(* task's poll at some epoch) and a claim the monitor writes (none,        *)
(* pending, taken). Before each poll the runner writes "task" and leaves   *)
(* the shard to the task's code; after the poll, and whenever the task's   *)
(* code calls into the runtime (a bracket: the mark of the call), it       *)
(* writes "loop", then reads its claim, with no fence between the two (a   *)
(* compiler fence only): none and it goes in; a claim pending it cancels   *)
(* with a compare-and-swap (pending to none) and goes in, a claim taken it *)
(* leaves the shard. A call reads the word it left first (its own last     *)
(* store) and writes it back as it ends, so the poll is outside again and  *)
(* can still be taken if it blocks after the call. One call, the push of a *)
(* task into the shard's empty LIFO slot, first tries a leaf push that     *)
(* never contests: it marks the call and reads its claim as a bracket      *)
(* does; with the claim none it goes in, pushes or finds it cannot (the    *)
(* slot is taken, the shard another), and leaves; then, or at once when a  *)
(* claim is up, it writes its word back. When it did not push, the call    *)
(* takes the full bracket. The monitor, seeing a runner in one poll too    *)
(* long, writes "pending", runs the heavy fence (membarrier: every         *)
(* thread's buffered stores reach memory), then reads the runner's word    *)
(* again: still the same "task", and the runner is outside the shard and   *)
(* will see the claim before it goes back in, so the monitor takes the     *)
(* shard with a compare-and-swap (pending to taken), unless the runner     *)
(* cancelled first, and hands it to a spare thread with a fresh slot;      *)
(* anything else, and it cancels. Neither side waits for the other: one of *)
(* the two swaps wins. A runner that sees "taken" never goes into the      *)
(* shard again (it is detached). Its task's code may still call into the   *)
(* runtime before its stack unwinds: a call around a nested `block_on`     *)
(* whose poll found the take ends, and puts the old slot back as the one   *)
(* the code runs outside of (`Bracket`'s drop), so the next call, a leaf   *)
(* push (`push_lifo_marked` reads that slot) or a bracket, marks the old   *)
(* slot and reads its claim. That claim stays taken for good: `Slot::take` *)
(* is its last write, the runner's `Slot::contest` swaps only from         *)
(* pending, and the monitor hands the shard on with a fresh slot           *)
(* (`Monitor::slot` in `monitor::take`) and never claims the old one       *)
(* again; so each such call backs out and stays out. The detached runner   *)
(* then drains the bound tasks it created and waits as a spare itself.     *)
(*                                                                          *)
(* What is modelled step by step: each thread's store buffer (in order),   *)
(* the flush of a buffer's oldest store at any time, the heavy fence as    *)
(* the flush of every buffer at once, each store and load of the protocol, *)
(* the hand-over to a spare, remote pushes, bound tasks. What is abstract: *)
(* the shard's owned part is the set of actors inside it; a task's work is *)
(* a step; the hand-over to a spare is one step (a release and an acquire).*)
(* Weaker orders than x86's are the release stores, acquire loads and     *)
(* fence pairing the code uses; this model is the store-load reordering    *)
(* the fence pair exists to stop.                                          *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    MaxPolls,      \* polls each runner makes per turn
    MaxHandoffs,   \* handoffs the monitor may make
    Pushes,        \* remote pushes of work to the shard
    Bounds,        \* tasks bound to the creating thread, pushed remotely
    BlockForever,  \* the first runner's first poll never returns
    DetachedCalls, \* calls into the runtime a detached thread's code still makes
    Mode           \* "ok" or a mutation (see the configurations)

Threads == {1, 2}
Creator == 1          \* the thread that made the shard: its bound tasks are its own
Mon == 3
Pusher == 4
Flusher == 0
Actors == Threads \cup {Mon}
Slots == 1..(1 + MaxHandoffs)

NONE == 0
PENDING == 1
TAKEN == 2
\* A store buffer holds this many stores; a store into a full one waits, as a CPU's does.
BufCap == 2
\* Calls into the runtime one poll makes.
MaxBrackets == 1

\* What actor `a` reads at (kind, slot): its own newest buffered store, else memory.
Newest(sq, k, s) ==
    {i \in DOMAIN sq : sq[i][1] = k /\ sq[i][2] = s
                       /\ \A j \in DOMAIN sq : (sq[j][1] = k /\ sq[j][2] = s) => j <= i}
ReadAt(mem, sq, k, s) ==
    IF Newest(sq, k, s) = {} THEN mem[s] ELSE sq[CHOOSE i \in Newest(sq, k, s) : TRUE][3]

\* A buffer's stores applied to memory in order.
RECURSIVE Apply(_, _, _)
Apply(mem, sq, k) ==
    IF sq = <<>> THEN mem
    ELSE Apply(IF Head(sq)[1] = k THEN [mem EXCEPT ![Head(sq)[2]] = Head(sq)[3]] ELSE mem,
               Tail(sq), k)

(* --algorithm Handoff
variables
    word = [s \in Slots |-> 0],          \* 0: in the loop; e > 0: in the poll of epoch e
    claim = [s \in Slots |-> NONE],
    buf = [a \in Actors |-> <<>>],
    inS = {1},                           \* actors inside the shard's owned part
    runnerSlot = 1,
    nextSlot = 2,
    assign = [t \in Threads |-> 0],      \* the slot a spare is handed, with the shard
    inbox = 0,
    pushed = 0,
    ran = 0,
    boundQ = 0,                          \* the shard's bound inbox
    boundBy = {},                        \* threads that ran a bound task
    handoffs = 0,
    cancels = 0,
    detachedCalls = 0,
    runnerCancels = 0,                   \* (record) a runner cancelled a pending claim
    leafBacks = 0,                       \* (record) a leaf push backed out on a claim
    gone = {},                           \* threads detached, not handed a shard since
    staleBacks = 0,                      \* (record) a detached thread's leaf push backed out
    runs = {1},                          \* threads whose context names a shard they run (`context::running`)
    ownerDetached = 0,                   \* (record) a detached thread's IO owner step went the foreign way
    markNoSlot = FALSE;                  \* an IO owner's mark read a thread's own slot the detach cleared

define
    \* At most one actor is inside the shard's owned part.
    Exclusive == Cardinality(inS) <= 1
    \* A thread that saw its shard taken stays out of it until handed it again.
    \* Its calls into the runtime on the slot it was taken from included.
    DetachedStaysOut ==
        /\ \A t \in Threads : pc[t] \in {"D_unwind", "D_bound", "D_drain", "D_spare", "Spare"} => t \notin inS
        /\ \A t \in gone : t \notin inS
    \* A bound task runs on the thread that created it, never elsewhere.
    BoundOnCreator == boundBy \subseteq {Creator}
    \* Every remote push is run, the first runner blocked for good or not.
    Rescued == <>(ran = Pushes)
    \* Witnesses.
    NoHandoff == handoffs = 0
    NoCancel == cancels = 0
    NoDetachedCall == detachedCalls = 0
    NoRunnerCancel == runnerCancels = 0
    NoLeafBack == leafBacks = 0
    NoStaleBack == staleBacks = 0
    NoOwnerDetached == ownerDetached = 0
    \* An IO owner's mark (`mark_call`) is made only by a thread that runs its shard, through its own slot.
    NoMarkWithoutSlot == ~markNoSlot
    \* The leaf push is part of the algorithm under these modes: the leaf and its mutations.
    Leaf == Mode \in {"leaf", "leaf_no_check", "leaf_keeps_mark", "taken_cleared"}
end define;

\* The memory: a buffer's oldest store reaches memory, at any time.
fair process flusher = Flusher
begin
Flush:
    while TRUE do
        with a \in {b \in Actors : buf[b] # <<>>} do
            if Head(buf[a])[1] = "word" then
                word[Head(buf[a])[2]] := Head(buf[a])[3];
            else
                claim[Head(buf[a])[2]] := Head(buf[a])[3];
            end if;
            buf[a] := Tail(buf[a]);
        end with;
    end while;
end process;

\* Other threads push work and bound tasks to the shard (its `Shared`), whoever runs it.
fair process pusher = Pusher
begin
P_push:
    while pushed < Pushes + Bounds do
        if pushed < Pushes then
            inbox := inbox + 1;
        else
            boundQ := boundQ + 1;
        end if;
        pushed := pushed + 1;
    end while;
end process;

fair process thread \in Threads
\* sl: the slot the thread runs with; e: its poll's epoch; polls: polls this turn; c: the claim it
\* read; br: calls this poll made; saved: the word a call read at its start, put back at its end.
\* dc: the calls its code made while detached.
variables sl = 0, e = 0, polls = 0, c = NONE, br = 0, saved = 0, dc = 0;
begin
T_begin:
    if self = 1 then
        sl := 1;
        goto R_loop;
    end if;
Spare:
    \* Waits to be handed a shard; the monitor left it, with every store it made.
    await assign[self] # 0;
    sl := assign[self];
    assign[self] := 0;
    gone := gone \ {self};
    runs := runs \cup {self};
    e := 0;
    polls := 0;
    inS := inS \cup {self};
R_loop:
    \* Inside the shard: its queues, then a poll, else asleep until work comes.
    if inbox > 0 then
        inbox := inbox - 1;
        ran := ran + 1;
        goto R_loop;
    elsif boundQ > 0 /\ (self = Creator \/ Mode = "bound_any") then
        boundQ := boundQ - 1;
        boundBy := boundBy \cup {self};
        goto R_loop;
    elsif polls < MaxPolls then
        polls := polls + 1;
        e := e + 1;
    else
        await inbox > 0 \/ (boundQ > 0 /\ (self = Creator \/ Mode = "bound_any"));
        goto R_loop;
    end if;
R_poll:
    \* word := (task, e), a release store; the task's code runs outside the shard.
    await Len(buf[self]) < BufCap;
    buf[self] := Append(buf[self], <<"word", sl, e>>);
    br := 0;
    inS := inS \ {self};
R_task:
    either
        await br < MaxBrackets;
        br := br + 1;
        goto B_store;
    or
        \* The call is the push of a task into the shard's LIFO slot: the leaf first.
        await Leaf /\ br < MaxBrackets;
        br := br + 1;
        goto L_store;
    or
        goto E_store;
    or
        await BlockForever /\ self = 1 /\ sl = 1 /\ e = 1;
        goto Blocked;
    end either;
B_store:
    \* A bracket: the task calls into the runtime (`Slot::mark`). It reads the word it left, its own
    \* newest store (the poll's "task"), and marks the call: word := loop, a compiler fence only.
    await Len(buf[self]) < BufCap;
    saved := ReadAt(word, buf[self], "word", sl);
    \* (MUTATION no_call_mark: the call reads its claim with no mark first.)
    if Mode # "no_call_mark" then
        buf[self] := Append(buf[self], <<"word", sl, 0>>);
    end if;
B_check:
    c := ReadAt(claim, buf[self], "claim", sl);
    if c = NONE \/ Mode = "no_bracket_check" then
        inS := inS \cup {self};
        goto B_in;
    end if;
B_cas:
    \* A claim is up: cancel it if still pending (a locked instruction, so the
    \* buffer drains first), or leave a shard that is taken.
    await buf[self] = <<>>;
    if claim[sl] = TAKEN then
        \* The call goes through the shard's remote paths instead.
        \* (MUTATION taken_cleared: the detaching runner clears the taken claim, as a reset of its
        \* slot would.)
        if Mode = "taken_cleared" then
            claim[sl] := NONE;
        end if;
        detachedCalls := 1;
        gone := gone \cup {self};
        \* `Here::detach`: the thread's context names no shard, and its own slot is cleared.
        runs := runs \ {self};
        goto D_unwind;
    else
        if claim[sl] = PENDING then
            runnerCancels := 1;
        end if;
        claim[sl] := NONE;
        inS := inS \cup {self};
    end if;
B_in:
    \* The runtime's work, inside the shard; then the call ends (`Slot::unmark`).
    inS := inS \ {self};
B_out:
    \* word := the word the call read at its start, a release store: outside the shard again.
    \* (MUTATION call_stays_marked: the call leaves its mark, so the word says loop for the rest
    \* of the poll.)
    if Mode # "call_stays_marked" then
        await Len(buf[self]) < BufCap;
        buf[self] := Append(buf[self], <<"word", sl, saved>>);
    end if;
    goto R_task;
E_store:
    \* The poll ends: word := loop, a compiler fence only.
    await Len(buf[self]) < BufCap;
    buf[self] := Append(buf[self], <<"word", sl, 0>>);
E_check:
    c := ReadAt(claim, buf[self], "claim", sl);
    if c = NONE \/ Mode = "no_poll_check" then
        inS := inS \cup {self};
        goto R_loop;
    end if;
E_cas:
    await buf[self] = <<>>;
    if claim[sl] = TAKEN then
        if Mode = "taken_cleared" then
            claim[sl] := NONE;
        end if;
        gone := gone \cup {self};
        runs := runs \ {self};
        goto D_unwind;
    else
        if claim[sl] = PENDING then
            runnerCancels := 1;
        end if;
        claim[sl] := NONE;
        inS := inS \cup {self};
        goto R_loop;
    end if;
L_store:
    \* The leaf push (`push_lifo_marked`, `Slot::mark_unless_claimed`): it reads the word it left,
    \* its own newest store, and marks the call as a bracket does: word := loop, a compiler fence.
    await Len(buf[self]) < BufCap;
    saved := ReadAt(word, buf[self], "word", sl);
    buf[self] := Append(buf[self], <<"word", sl, 0>>);
L_check:
    \* It reads its claim: none, and it goes in.
    \* (MUTATION leaf_no_check: it goes in without reading the claim.)
    c := ReadAt(claim, buf[self], "claim", sl);
    if c = NONE \/ Mode = "leaf_no_check" then
        inS := inS \cup {self};
        goto L_in;
    end if;
L_claimed:
    \* A claim is up: the leaf never contests one, it backs out.
    leafBacks := 1;
    if self \in gone then
        staleBacks := 1;
    end if;
    goto L_back;
L_in:
    \* Inside: the push into the empty slot, or none (the slot holds a task, the shard is
    \* another); either way the leaf leaves the shard at once.
    inS := inS \ {self};
    either
        \* Pushed: word := the word it read, a release store, and the call is done.
        await Len(buf[self]) < BufCap;
        buf[self] := Append(buf[self], <<"word", sl, saved>>);
        goto R_task;
    or
        \* Not its push: it backs out, and the call takes the full bracket.
        goto L_back;
    end either;
L_back:
    \* word := the word it read, a release store: outside again, as before the leaf. Then the
    \* full bracket, which reads that word as its own and puts it back as it ends.
    \* (MUTATION leaf_keeps_mark: it backs out with its mark still up, so the bracket reads
    \* "loop" as the word to put back, and the rest of the poll says loop.)
    if Mode # "leaf_keeps_mark" then
        await Len(buf[self]) < BufCap;
        buf[self] := Append(buf[self], <<"word", sl, saved>>);
    end if;
    goto B_store;
Blocked:
    await FALSE;
D_unwind:
    \* Detached: the rest of the stack unwinds, and its code may call into the runtime again on the
    \* slot it was taken from (`Bracket`'s drop put it back): a leaf push, a bracket or an IO owner's
    \* step, each of which reads that slot's claim, taken for good, and stays out (see the opening
    \* story).
    if dc < DetachedCalls then
        dc := dc + 1;
        either
            await Leaf;
            goto L_store;
        or
            goto B_store;
        or
            goto O_io;
        end either;
    else
        goto D_bound;
    end if;
O_io:
    \* An IO owner's step: it looks at the shard this thread runs (`context::running`) before it marks
    \* its call (`mark_call`, through the thread's own slot). A detached thread runs none, so the step
    \* marks nothing and goes the way of any other thread: a bracket on the slot `out` names, whose
    \* claim is taken, so it stays out.
    \* (MUTATION owner_skips_running: the step marks its call with no look at the shard the thread runs.)
    if self \in runs \/ Mode = "owner_skips_running" then
        \* `mark_call` reads the thread's own slot, which the detach cleared on a detached thread.
        if self \notin runs then
            markNoSlot := TRUE;
        end if;
    else
        ownerDetached := 1;
    end if;
    goto B_store;
D_bound:
    \* The creator runs its bound tasks on a shard of its own.
    if self = Creator then
D_drain:
        while boundQ > 0 do
            boundQ := boundQ - 1;
            boundBy := boundBy \cup {self};
        end while;
    end if;
D_spare:
    goto Spare;
end process;

fair process monitor = Mon
variables ms = 0, mv = 0;
begin
M_tick:
    while handoffs < MaxHandoffs do
        \* A runner in one poll, and a spare to hand its shard to.
        await word[runnerSlot] > 0 /\ \E t \in Threads : pc[t] = "Spare";
        ms := runnerSlot;
        mv := word[ms];
M_claim:
        await Len(buf[Mon]) < BufCap;
        buf[Mon] := Append(buf[Mon], <<"claim", ms, PENDING>>);
M_fence:
        \* membarrier: every thread's buffered stores reach memory.
        if Mode # "no_fence" then
            word := Apply(Apply(word, buf[1], "word"), buf[2], "word");
            claim := Apply(claim, buf[Mon], "claim");
            buf := [a \in Actors |-> <<>>];
        end if;
M_check:
        if ReadAt(word, buf[Mon], "word", ms) # mv /\ Mode # "no_recheck" then
            \* The runner is back in, or in a bracket: cancel (pending to none).
            await buf[Mon] = <<>>;
            if claim[ms] = PENDING then
                claim[ms] := NONE;
            end if;
            cancels := 1;
            goto M_tick;
        end if;
M_take:
        \* Pending to taken, a compare-and-swap: lost when the runner cancelled first.
        await buf[Mon] = <<>>;
        if claim[ms] = PENDING then
            claim[ms] := TAKEN;
            inS := inS \cup {Mon};
        else
            cancels := 1;
            goto M_tick;
        end if;
M_give:
        with t \in {u \in Threads : pc[u] = "Spare"} do
            if Mode = "slot_reuse" then
                \* The spare is handed the old slot, its claim cleared, in memory.
                claim[ms] := NONE;
                assign[t] := ms;
                runnerSlot := ms;
            else
                assign[t] := nextSlot;
                runnerSlot := nextSlot;
                nextSlot := nextSlot + 1;
            end if;
            inS := inS \ {Mon};
            handoffs := handoffs + 1;
        end with;
    end while;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

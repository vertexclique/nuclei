----------------------------- MODULE SideBiased -----------------------------
(***************************************************************************)
(* The side guard biased to its owner (Phase 2 of the network-path plan),  *)
(* on a machine whose stores wait in a store buffer, as x86's do. No code  *)
(* is written yet; the planned code is src/io/side.rs: `Side::try_hold`    *)
(* and `Side::poll` (the takes), `Side::contend` (the poll that finds the  *)
(* side held), `Side::defer` and `Side::take_for_request` (the calls that  *)
(* leave DEFER and REQ), `Held::release_or_again` (the release); the     *)
(* fence is `heavy` in src/rt/handoff/fence.rs; the shard's move is        *)
(* `Slot::claim`, `Slot::take` and `Slot::come_back` in                    *)
(* src/rt/handoff/slot.rs, driven by src/rt/handoff/monitor.rs.            *)
(*                                                                          *)
(* Who acts. A socket's side has an owner: the thread running the socket's *)
(* home shard. The owner serves the driver's edges (completions) and most  *)
(* polls. Foreign threads call on the same side from outside the home      *)
(* shard (a `Send` handle polled from another thread, a buffer given back  *)
(* there, a dropped handle's request): a poll, a defer or a request. A    *)
(* monitor may move the home shard to a spare thread once (the handoff),   *)
(* and a flusher stands for the memory: at any moment the oldest store of *)
(* any buffer lands.                                                       *)
(*                                                                          *)
(* What can happen. In biased mode the owner takes the side with a plain   *)
(* store of own := 1 (into its buffer), a compiler fence only, and a plain *)
(* load of intent: 0, it holds; 1, it stores own := 0 and takes the side   *)
(* with today's atomic word instead. It lets go with a plain store of     *)
(* own := 0 and a plain load of intent; only when intent is 1 does it look *)
(* at the flag word, and then with a locked instruction, which drains its  *)
(* buffer first. Finding work left there (WAITING, READY, DEFER or REQ),   *)
(* it takes the side again through today's word and does the work as      *)
(* today's holder does. A foreign call raises intent with a locked        *)
(* instruction, runs the heavy fence (every buffer drains), and reads own: *)
(* 0, it switches the side to shared mode for good and takes it with      *)
(* today's word; 1, it leaves its work (a poll its waker in the contended  *)
(* set and WAITING, a defer DEFER, a request REQ), looks once more at own  *)
(* as today's poll looks once more at the word, and then a poll sleeps     *)
(* until the holder wakes it while a defer or a request goes on. In shared *)
(* mode everyone uses today's word: a take is one fetch_or of BUSY (with  *)
(* the call's own flag), a holder lets go with one compare-and-swap, and   *)
(* flags others left make it fail and are taken with one swap and done.   *)
(* The monitor moves the shard only through the slot protocol: it claims   *)
(* the owner's slot while the owner is outside its shard (in a task's      *)
(* code), runs the heavy fence, and takes the shard unless the owner came  *)
(* back first and cancelled; the old owner, back, finds its shard taken   *)
(* and acts as a foreign thread from then on.                              *)
(*                                                                          *)
(* A heavy fence may fail (`fence::heavy_retried` tries it eight times).  *)
(* A foreign call whose fence failed reads nothing the fence was to order *)
(* (`Side::unserved`): after a full fence of its own it reads the owner's *)
(* claim; with none, no owner can hold biased (a later claimant sees      *)
(* intent), so the side goes shared and is taken through the word; with  *)
(* one, it raises intent's UNFENCED bit, leaves its work in the word (a   *)
(* poll its waker and WAITING), asks the owner's shard to take and let go *)
(* of the side (a nudge, which reaches it once the claim's shard is       *)
(* recorded; a claimant not recorded yet takes the side next anyway), and *)
(* a poll sleeps. The owner, letting go of `own` at its fallback or its   *)
(* release, switches the side to shared when it sees UNFENCED            *)
(* (`Side::switch_if_unfenced`), so the woken poll takes through the word. *)
(*                                                                          *)
(* What must never happen. Two threads hold the side at once. Work a       *)
(* foreign call left (a waiting poll, a DEFER, a REQ, an edge's READY) is  *)
(* never done. A thread waits for another other than by sleeping until it *)
(* is woken. A side in shared mode goes back to biased mode.               *)
(*                                                                          *)
(* What is modelled step by step: each store and load of own, intent,     *)
(* mode and the flag word; each thread's buffer of two stores, oldest     *)
(* first; the heavy fence as the drain of every buffer at once; a locked   *)
(* instruction as the drain of its own thread's buffer before it acts;    *)
(* the contended set's registration and take, the wakes and the sleeps;   *)
(* the slot's claim, the fence and the take, and the owner's way back in.  *)
(*                                                                          *)
(* What is abstract. The slot's own words are atomic here: Handoff.tla     *)
(* checks them on the same store buffers, and this model takes from it     *)
(* what its take guarantees: the owner is outside its shard when the take  *)
(* lands, every store it made before the fence has reached memory, and it  *)
(* makes no store to the side until it comes back, which a pending claim  *)
(* cancels and a taken one turns into a foreign thread. The owner's edges  *)
(* stand for every take on the owner (its own polls take the side the     *)
(* same way; one that finds it held in shared mode is today's contended   *)
(* poll, which Side.tla checks). A foreign call's step is one step. DEFER  *)
(* and REQ are work a holder does when its swap takes the bit; what that   *)
(* work is (the deferred bits, the requests and both sides a request       *)
(* needs) is Side.tla's and Requests.tla's.                                *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC                                 \* numbers, sequences (buffers, calls), set sizes, TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Edges,      \* the driver edges the home shard serves, one take each
    Foreign,    \* the threads that call on the side from outside its home shard
    Calls,      \* the calls each of those threads makes, in order: "poll", "defer" or "req"
    Handoffs,   \* 0, or 1 when the monitor may move the home shard once
    FenceFails, \* "never", "maybe" (each foreign call's heavy fence may fail) or "always"
    OldCalls,   \* the calls the first owner makes once it is a foreign thread
    Mutation    \* "none", or the name of the one rule this run takes away

First == 1          \* the thread that runs the home shard first
Spare == 2          \* the spare thread the monitor hands the shard to
Flusher == 0        \* the memory, draining buffers
Mon == 9            \* the monitor that may move the shard
Threads == {First} \cup Foreign \cup (IF Handoffs > 0 THEN {Spare} ELSE {})   \* every thread that touches the side

NONE == 0           \* a slot's claim: none, the shard is its runner's
PENDING == 1        \* the monitor is about to take the shard
TAKEN == 2          \* the monitor took the shard
BufCap == 2         \* a store buffer holds this many stores; a store into a full one waits for its oldest

\* The value thread buffer `sq` gives a load of word `w`: its own newest store of `w`, else memory's.
ReadAt(m, sq, w) ==                                                          \* the load's value:
    LET idx == {i \in DOMAIN sq : sq[i][1] = w}                              \* the buffered stores of `w`
    IN IF idx = {} THEN m[w]                                                 \* none: memory answers
       ELSE sq[CHOOSE i \in idx : \A j \in idx : j <= i][2]                  \* else the newest one

\* Memory `m` after the stores of buffer `sq` land, oldest first.
RECURSIVE Apply(_, _)                                                        \* Apply calls itself, one store at a time
Apply(m, sq) ==                                                              \* memory once the buffer landed:
    IF sq = <<>> THEN m                                                      \* nothing left to land
    ELSE Apply([m EXCEPT ![Head(sq)[1]] = Head(sq)[2]], Tail(sq))            \* the oldest lands, then the rest

\* Memory `m` after every buffer of the threads in `S` lands, one thread after another.
RECURSIVE DrainAll(_, _, _)                                                  \* DrainAll calls itself, one thread at a time
DrainAll(m, b, S) ==                                                         \* memory once every buffer landed:
    IF S = {} THEN m                                                         \* every buffer landed
    ELSE LET t == CHOOSE u \in S : \A v \in S : u <= v                       \* the lowest thread left
         IN DrainAll(Apply(m, b[t]), b, S \ {t})                             \* its buffer lands, then the others'

(* --algorithm SideBiased                                                      \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    mem = [own |-> 0, mode |-> "biased"],   \* memory's copy of the plain words: own, and the side's mode
    buf = [t \in Threads |-> <<>>],         \* each thread's store buffer, oldest first, of <<word, value>> pairs
    intent = 0,                             \* the foreign word: only locked instructions write it, so it is never buffered
    unfenced = FALSE,                       \* intent's UNFENCED bit: a foreign call's heavy fence failed
    ownerClaim = "none",                    \* the owner's claim of the side: "none", "cas" (won, shard not recorded), "cell"
    nudges = 0,                             \* nudges posted to the owner's shard and not served yet
    busy = FALSE,                           \* today's flag word, written only by locked instructions: BUSY ...
    ready = FALSE,                          \* ... READY, an edge came while a holder held the word ...
    waiting = FALSE,                        \* ... WAITING, a poll sleeps until the holder wakes it ...
    defer = FALSE,                          \* ... DEFER, side-local work left for the holder ...
    req = FALSE,                            \* ... and REQ, a request left for the holder
    contended = {},                         \* the threads whose wakers are in the side's contended set
    woken = {},                             \* the threads woken and not polled again yet
    owner = First,                          \* the thread running the home shard
    claim = [t \in Threads |-> NONE],       \* each runner's slot claim; a new runner gets a fresh slot
    edgesLeft = Edges,                      \* the edges the home shard has still to serve
    dirty = FALSE,                          \* an edge came that no driver's part has served since
    todo = [t \in Threads |->               \* the calls each thread still makes from outside the home shard:
        IF t \in Foreign THEN Calls         \* a foreign thread's own,
        ELSE IF t = First THEN OldCalls     \* the first owner's once its shard is taken,
        ELSE <<>>],                         \* and none for the spare
    fresh = [t \in Threads |-> FALSE],      \* the thread's next take starts a new call or follows a wake
    handoffs = 0,                           \* the times the monitor moved the shard
    left = FALSE,                           \* (record) a foreign call saw own = 1 and left its work
    switched = FALSE,                       \* (record) a foreign call switched the side to shared mode
    releaseWoke = FALSE,                    \* (record) the owner's release took the side again and woke a waiting poll
    retakeWork = FALSE,                     \* (record) the owner's release took the side again and did a DEFER or a REQ
    unservedSeen = FALSE,                   \* (record) a foreign call's fence failed with an owner claimed
    failShared = FALSE,                     \* (record) a foreign call's fence failed with no owner claimed: shared
    nudgePass = FALSE,                      \* (record) the owner took the side for a nudge
    switchedUnfenced = FALSE;               \* (record) the owner switched the side on UNFENCED

define                                                                       \* names the code and the properties use:
    \* Every thread and the monitor finished.
    Quiet == (\A t \in Threads : pc[t] = "Done") /\ pc[Mon] = "Done"         \* no step left
    \* Every buffer is empty.
    Drained == \A t \in Threads : buf[t] = <<>>                              \* no store waiting
    \* Every thread but the owner finished.
    CallersDone == \A t \in Threads : t = owner \/ pc[t] = "Done"           \* no call can come
    \* Some work is left in the flag word for a holder.
    Flagged == ready \/ waiting \/ defer \/ req                              \* READY, WAITING, DEFER or REQ is up
end define;                                                                  \* end of the names

\* A plain store of `v` into word `w`: it joins this thread's buffer.
macro Store(w, v) begin                                                      \* its steps:
    if Len(buf[self]) = BufCap then                                          \* a full buffer: the CPU waits for its oldest store,
        mem := [mem EXCEPT ![Head(buf[self])[1]] = Head(buf[self])[2]];      \* which lands in memory,
        buf[self] := Append(Tail(buf[self]), <<w, v>>);                      \* and the new store joins the end
    else                                                                     \* room left:
        buf[self] := Append(buf[self], <<w, v>>);                            \* the new store joins the end
    end if;                                                                  \* either way the store waits in the buffer
end macro;                                                                   \* end of the plain store

\* A locked instruction: this thread's buffer drains before it acts.
macro Lock() begin                                                           \* its steps:
    mem := Apply(mem, buf[self]);                                            \* every store this thread buffered lands, oldest first
    buf[self] := <<>>;                                                       \* and its buffer is empty
end macro;                                                                   \* end of the locked instruction

\* The heavy fence: as if every running thread ran a full fence at this moment.
macro HeavyFence() begin                                                     \* its steps:
    mem := DrainAll(mem, buf, Threads);                                      \* every buffered store of every thread lands
    buf := [t \in Threads |-> <<>>];                                         \* and every buffer is empty
end macro;                                                                   \* end of the heavy fence

\* The memory: the oldest store of any buffer lands, at any time, until everything is finished.
fair process flusher = Flusher                                               \* weakly fair: a buffered store lands in the end
begin                                                                        \* the memory's steps:
Flush:                                                                       \* one store lands
    while ~(Quiet /\ Drained) do                                             \* while anything runs or any store waits
        with t \in {u \in Threads : buf[u] # <<>>} do                        \* some thread with a buffered store:
            mem := [mem EXCEPT ![Head(buf[t])[1]] = Head(buf[t])[2]];        \* its oldest store lands
            buf[t] := Tail(buf[t]);                                          \* and leaves the buffer
        end with;                                                            \* one store per step
    end while;                                                               \* everything finished and landed
end process;                                                                 \* end of the memory

\* The monitor: it may move the home shard to the spare once, through the slot protocol.
fair process monitor = Mon                                                   \* weakly fair, like every process
variables ms = First;                                                        \* the runner whose slot it claimed
begin                                                                        \* the monitor's steps:
M_watch:                                                                     \* the monitor watches the owner
    while handoffs < Handoffs do                                             \* a move is still allowed
        either                                                               \* either the owner sits in a task's code too long:
            \* MUTATION move_mid_take: the claim does not wait for the owner to be outside its shard.
            await edgesLeft > 0                                              \* there is still work to move,
                  /\ IF Mutation = "move_mid_take"                           \* and the owner is
                     THEN pc[owner] \in {"O_mode", "O_own", "O_intent", "O_serve", "O_rel"}   \* (mutated) inside a take,
                     ELSE pc[owner] = "O_out";                               \* else outside its shard
            claim[owner] := PENDING;                                         \* the claim on its slot (`Slot::claim`)
            ms := owner;                                                     \* remembered for the take
        or                                                                   \* or there is nothing left to move:
            await edgesLeft = 0;                                             \* every edge was begun,
            goto Done;                                                       \* so the monitor stops watching
        end either;                                                          \* a claim is up
M_fence:                                                                     \* the monitor's heavy fence
        \* MUTATION handoff_no_drain: the move does not drain the old owner's buffer.
        if Mutation # "handoff_no_drain" then                                \* the heavy fence (`heavy`):
            HeavyFence();                                                    \* every buffer, the owner's too, drains
        end if;                                                              \* the fence is done
M_take:                                                                      \* the monitor's take
        if claim[ms] = PENDING then                                          \* the owner did not come back and cancel:
            claim[ms] := TAKEN;                                              \* the take (`Slot::take`) lands,
            owner := Spare;                                                  \* the spare runs the shard from now on,
            handoffs := handoffs + 1;                                        \* and the move is counted
        end if;                                                              \* else the owner cancelled first: nothing moves
    end while;                                                               \* watch again
end process;                                                                 \* end of the monitor

\* A thread: the owner's code, a foreign thread's code, and today's release that both use.
fair process thread \in Threads                                              \* one process per thread, weakly fair
variables                                                                    \* each thread's own state:
    role = "owner",                                                          \* whose code the current take runs: "owner" or "foreign"
    kind = "poll",                                                           \* the foreign call being made: "poll", "defer" or "req"
    owed = {},                                                               \* contended wakers a shared holder took, to wake once it lets go
    sawReady = FALSE,                                                        \* the READY a release's swap took
    sawWaiting = FALSE,                                                      \* the WAITING a release's swap took
    prevBusy = FALSE,                                                        \* what a fetch_or of a flag said of BUSY
    retaking = FALSE;                                                        \* the owner holds the side again for work its release found
begin                                                                        \* a thread's steps:
T_begin:                                                                     \* each thread goes to its part
    if self = First then goto O_out;                                         \* the first owner starts outside its shard
    elsif self = Spare then goto S_wait;                                     \* the spare waits to be handed the shard
    else goto F_loop;                                                        \* a foreign thread starts calling
    end if;                                                                  \* every thread has its part
S_wait:                                                                      \* the spare waits
    await owner = self \/ pc[Mon] = "Done";                                  \* handed the shard, or no move will come
    if owner # self then goto Done; end if;                                  \* no move: nothing to do
O_out:                                                                       \* the owner, outside its shard
    \* The owner is outside its shard (in a task's code, or its run loop waiting for work); it comes back in through
    \* its slot (`Slot::come_back`), for an edge or a nudge.
    \* (With fences that never fail no nudge comes, and the owner is finished after its edges.)
    await claim[self] = TAKEN \/ edgesLeft > 0 \/ nudges > 0                \* work, its shard taken,
          \/ CallersDone \/ FenceFails = "never";                            \* or no nudge can come
    if claim[self] = TAKEN then goto F_loop;                                 \* its shard was taken: a foreign thread from now on
    elsif edgesLeft = 0 /\ nudges = 0 then goto Done;                        \* nothing left and no nudge to come: finished
    else                                                                     \* back in:
        if claim[self] = PENDING then claim[self] := NONE; end if;           \* a pending claim is cancelled first
        if nudges > 0 then                                                   \* a nudge first (`IoOp::RingKick`):
            nudges := nudges - 1;                                            \* taken,
            nudgePass := TRUE;                                               \* (record) a take and a release for it
        else                                                                 \* else an edge:
            edgesLeft := edgesLeft - 1;                                      \* begun
        end if;                                                              \* either takes the side like an edge
        dirty := TRUE;                                                       \* it brought work for the driver's part
        role := "owner";                                                     \* and runs the owner's code
    end if;                                                                  \* the owner is inside its shard
O_mode:                                                                      \* the owner's take: mode
    \* The owner's take (`Side::try_hold`, `Side::poll`): a plain load of mode first.
    if ReadAt(mem, buf[self], "mode") = "shared" then goto O_shared;         \* shared mode: today's word at once
    \* (The claim matters only to a call whose fence failed: with fences that never fail it is not modelled.)
    elsif ownerClaim = "none" /\ FenceFails # "never" then goto O_claim;     \* its first take claims the side
    end if;                                                                  \* else it takes biased
O_own:                                                                       \* the owner's take: own := 1
    Store("own", 1);                                                         \* own := 1, a plain store into the buffer
O_intent:                                                                    \* the owner's take: intent
    \* A compiler fence only, then a plain load of intent.
    \* MUTATION owner_no_intent: the owner holds without reading intent.
    \* MUTATION mode_skips_fallback: with intent = 1 the owner reads mode and holds while it says biased.
    if Mutation # "owner_no_intent"                                          \* the owner reads intent,
       /\ intent = 1                                                         \* a foreign thread came,
       /\ ~(Mutation = "mode_skips_fallback"                                 \* and (unless mutated so that a mode
            /\ ReadAt(mem, buf[self], "mode") = "biased") then               \* still biased overrules it)
        goto O_undo;                                                         \* the owner falls back to today's word
    end if;                                                                  \* else it holds the side, biased
O_serve:                                                                     \* the owner holds the side, biased
    dirty := FALSE;                                                          \* holding: the driver's part for its edge
    \* MUTATION switch_before_release: the owner switches on UNFENCED before its own := 0, still holding.
    if Mutation = "switch_before_release" /\ unfenced then                  \* (mutated) the switch first:
        Store("mode", "shared");                                             \* mode := shared, while own is still 1
    end if;                                                                  \* else nothing here
O_rel:                                                                       \* the owner's release: own := 0
    \* The owner's release (`Held::release_or_again`): a plain store of own := 0.
    Store("own", 0);                                                         \* own := 0 joins the buffer
    \* MUTATION release_no_look: the release does not look for work a foreign thread left.
    if Mutation = "release_no_look" then goto O_out; end if;                 \* (mutated) straight back to the task
O_look:                                                                      \* the release looks at intent
    if intent = 0 then goto O_out;                                           \* a plain load of intent: no foreign thread came
    elsif FenceFails = "never" then goto O_flags;                            \* (no UNFENCED can come: no switch)
    end if;                                                                  \* else the switch first
O_rswitch:                                                                   \* the release's switch on UNFENCED (`switch_if_unfenced`)
    \* MUTATION never_switch: the owner never switches the side on UNFENCED.
    if unfenced /\ Mutation \notin {"never_switch", "switch_before_release"} then   \* a plain load of intent's UNFENCED bit:
        Store("mode", "shared");                                             \* mode := shared, a release store after own := 0
        switchedUnfenced := TRUE;                                            \* (record)
    end if;                                                                  \* then the locked look
O_flags:                                                                     \* the release looks at the flag word
    \* intent = 1: the flag word is read with a locked instruction, which drains own := 0 first.
    \* MUTATION release_plain_look: the flag word is read with a plain load, ahead of own := 0.
    if Mutation # "release_plain_look" then                                  \* the locked look:
        Lock();                                                              \* this thread's buffer drains first
    end if;                                                                  \* then the flag word is read
    if ~Flagged then goto O_out; end if;                                     \* no work left: the edge is finished
O_retake:                                                                    \* the release takes the side again
    \* MUTATION release_no_retake: work found, the release only wakes the contended and takes the side no more.
    if Mutation = "release_no_retake" then                                   \* (mutated) without the side:
        Lock();                                                              \* a locked take of WAITING,
        if waiting then                                                      \* a waiting poll:
            waiting := FALSE;                                                \* WAITING taken,
            woken := woken \cup contended;                                   \* every contended waker woken,
            contended := {};                                                 \* and the set empty;
        end if;                                                              \* DEFER and REQ stay up
        goto O_out;                                                          \* and the edge is finished
    else                                                                     \* else today's take, one fetch_or of BUSY:
        Lock();                                                              \* a locked instruction
        if busy then goto O_out;                                             \* held: the holder does the work as it lets go
        else busy := TRUE; retaking := TRUE; goto X_cas;                     \* free: the owner holds it and does the work
        end if;                                                              \* either way the release is over
    end if;                                                                  \* the left work has a holder
O_undo:                                                                      \* the owner's fallback: own := 0
    Store("own", 0);                                                         \* the fallback: own := 0 undoes the take
    if FenceFails = "never" then goto O_shared; end if;                      \* (no UNFENCED can come: no switch)
O_uswitch:                                                                   \* the fallback's switch on UNFENCED (`switch_if_unfenced`)
    if unfenced /\ Mutation # "never_switch" then                           \* a plain load of intent's UNFENCED bit:
        Store("mode", "shared");                                             \* mode := shared, a release store after own := 0
        switchedUnfenced := TRUE;                                            \* (record)
    end if;                                                                  \* then today's word
O_shared:                                                                    \* the owner's take through today's word
    \* Today's take: one fetch_or of BUSY | READY (the driver's own READY is masked).
    Lock();                                                                  \* a locked instruction
    if busy then                                                             \* held by another:
        ready := TRUE;                                                       \* READY is left for the holder
        goto O_out;                                                          \* and the owner goes on, waiting for nobody
    else                                                                     \* free:
        busy := TRUE;                                                        \* the owner holds the word
    end if;                                                                  \* the owner holds the side, shared
O_sserve:                                                                    \* the owner holds the side, shared
    dirty := FALSE;                                                          \* holding: the driver's part for its edge
    goto X_cas;                                                              \* then today's release
O_claim:                                                                     \* the owner's claim (`Side::role`), its first take only
    Lock();                                                                  \* a compare-and-swap of the owner word
    ownerClaim := "cas";                                                     \* won; its shard is not recorded yet
O_cell:                                                                      \* the claim's shard recorded (`claimed`)
    ownerClaim := "cell";                                                    \* a nudge can reach it from now on
    goto O_own;                                                              \* then the biased take
F_loop:                                                                      \* a foreign thread's next call
    if todo[self] = <<>> then goto Done;                                     \* no call left: finished
    else                                                                     \* the next call:
        kind := Head(todo[self]);                                            \* its kind,
        todo[self] := Tail(todo[self]);                                      \* begun,
        fresh[self] := TRUE;                                                 \* with a new take,
        role := "foreign";                                                   \* running a foreign thread's code
    end if;                                                                  \* the call takes the side
F_mode:                                                                      \* the foreign take: mode
    \* A foreign take (`Side::poll`, `Side::defer`, `Side::take_for_request`): a plain load of mode first.
    fresh[self] := FALSE;                                                    \* the take begins
    if ReadAt(mem, buf[self], "mode") = "shared" then goto F_take; end if;   \* shared mode: today's word at once
F_intent:                                                                    \* the foreign take: intent := 1
    Lock();                                                                  \* intent := 1 is a locked instruction,
    intent := 1;                                                             \* so it is in memory at once
F_fence:                                                                     \* the foreign take: the heavy fence
    \* MUTATION no_fence: the foreign take runs no heavy fence.
    \* MUTATION unfenced_reads_own: a foreign call whose fence failed reads own as if it had worked.
    either                                                                   \* the fence works (`heavy_retried`):
        await Mutation = "no_fence" \/ FenceFails # "always";               \* when it can,
        if Mutation # "no_fence" then                                        \* the heavy fence:
            HeavyFence();                                                    \* every buffer, the owner's too, drains
        end if;                                                              \* the fence is done
    or                                                                       \* or it fails, eight times over:
        await Mutation # "no_fence" /\ FenceFails # "never";                \* when it may,
        if Mutation # "unfenced_reads_own" then goto F_unserved; end if;     \* nothing it was to order may be read
    end either;                                                              \* (mutated: on to own anyway)
F_own:                                                                       \* the foreign take: own
    if ReadAt(mem, buf[self], "own") = 1 then goto F_lreg; end if;           \* a plain load of own: 1, the owner holds the side
F_switch:                                                                    \* the switch to shared mode
    \* own = 0: the side goes to shared mode for good.
    \* MUTATION foreign_stays_biased: the thread holds through its intent alone, with no switch and no BUSY.
    if Mutation = "foreign_stays_biased" then goto F_bstep;                  \* (mutated) it holds now
    else                                                                     \* else:
        Store("mode", "shared");                                             \* mode := shared, a plain store
        switched := TRUE;                                                    \* (record)
    end if;                                                                  \* then today's take
F_take:                                                                      \* today's take
    \* Today's take: one fetch_or of BUSY, with DEFER for a defer and REQ for a request.
    Lock();                                                                  \* a locked instruction
    if kind = "defer" then defer := TRUE;                                    \* a defer's DEFER goes up with it,
    elsif kind = "req" then req := TRUE;                                     \* a request's REQ too
    end if;                                                                  \* (a poll raises no flag)
    if ~busy then busy := TRUE; goto F_step;                                 \* free: the thread holds the side
    elsif kind # "poll" then goto F_loop;                                    \* held: the flag is the holder's work, and the call goes on
    end if;                                                                  \* held, for a poll: it contends
F_reg:                                                                       \* today's contended poll: its waker
    \* Held (today's `Side::contend`): the waker joins the contended set.
    Lock();                                                                  \* its registration is a compare-and-swap
    contended := contended \cup {self};                                      \* the waker is in the set
F_flag:                                                                      \* today's contended poll: WAITING
    Lock();                                                                  \* one fetch_or of WAITING,
    prevBusy := busy;                                                        \* which says whether the side was still held
    waiting := TRUE;                                                         \* and leaves WAITING for the holder
    if prevBusy then goto F_park; end if;                                    \* still held: the holder will wake it
F_retake:                                                                    \* today's contended poll: one more take
    Lock();                                                                  \* free meanwhile: one more fetch_or of BUSY
    if busy then goto F_park;                                                \* held again: the holder will wake it
    else busy := TRUE; goto F_step;                                          \* free: the thread holds the side
    end if;                                                                  \* either way the take is over
F_lreg:                                                                      \* the work left to the owner: a poll's waker
    \* own = 1: the owner holds the side; the work is left as today, a poll's waker first.
    if kind = "poll" then                                                    \* a poll:
        Lock();                                                              \* the registration is a compare-and-swap
        contended := contended \cup {self};                                  \* the waker is in the set
    end if;                                                                  \* a defer or a request registers nothing
    left := TRUE;                                                            \* (record)
F_lflag:                                                                     \* the work left to the owner: its flag
    Lock();                                                                  \* one fetch_or of the call's flag,
    prevBusy := busy;                                                        \* which says whether a shared holder holds the word,
    if kind = "poll" then waiting := TRUE;                                   \* WAITING for a poll,
    elsif kind = "defer" then defer := TRUE;                                 \* DEFER for a defer,
    else req := TRUE;                                                        \* REQ for a request,
    end if;                                                                  \* left for the holder
F_lrecheck:                                                                  \* the look once more at own
    \* As today's poll looks once more: free now, it takes the side; held, the holder does the work.
    if ~prevBusy /\ ReadAt(mem, buf[self], "own") = 0 then goto F_switch;    \* a plain load of own: 0, the side came free
    elsif kind # "poll" then goto F_loop;                                    \* held: a defer or a request goes on
    else goto F_park;                                                        \* held: a poll sleeps
    end if;                                                                  \* the leave is over
F_unserved:                                                                  \* a failed fence (`Side::unserved`): the claim
    Lock();                                                                  \* a full fence of this thread's own
    if ownerClaim = "none" then                                              \* no owner claimed: none holds biased,
        failShared := TRUE;                                                  \* (record)
        goto F_switch;                                                       \* so the side goes shared, through the word
    end if;                                                                  \* an owner claimed: its work is left
F_unfence:                                                                   \* UNFENCED up
    Lock();                                                                  \* a locked fetch_or of intent's UNFENCED bit
    unfenced := TRUE;                                                        \* the owner switches when it sees it
    unservedSeen := TRUE;                                                    \* (record)
F_ureg:                                                                      \* a poll's waker (`leave_waker`)
    if kind = "poll" then                                                    \* a poll:
        Lock();                                                              \* its registration is a compare-and-swap
        contended := contended \cup {self};                                  \* the waker is in the set
    end if;                                                                  \* a defer or a request registers nothing
F_uflag:                                                                     \* its flag, left in the word
    Lock();                                                                  \* one fetch_or of the call's flag:
    if kind = "poll" then waiting := TRUE;                                   \* WAITING for a poll,
    elsif kind = "defer" then defer := TRUE;                                 \* DEFER for a defer,
    else req := TRUE;                                                        \* REQ for a request
    end if;                                                                  \* left for the owner
F_nudge:                                                                     \* the nudge
    \* MUTATION nudge_lost: the nudge never reaches the owner's shard.
    if ownerClaim = "cell" /\ Mutation # "nudge_lost" then                  \* the claim's shard is recorded:
        nudges := nudges + 1;                                                \* it is asked to take and let go of the side
    end if;                                                                  \* (a claimant not recorded yet takes it next anyway)
    if kind # "poll" then goto F_loop; end if;                               \* a defer or a request goes on; a poll sleeps
F_park:                                                                      \* asleep until woken
    \* The poll returned Pending: the thread sleeps until a holder wakes it.
    await self \in woken;                                                    \* a wake came
    woken := woken \ {self};                                                 \* it is taken,
    fresh[self] := TRUE;                                                     \* and the next take follows it
    goto F_mode;                                                             \* the task is polled again
F_step:                                                                      \* the foreign call holds the side
    goto X_cas;                                                              \* holding: the call's step, then today's release
F_bstep:                                                                     \* (mutated) the foreign call holds through intent
    goto F_brel;                                                             \* (mutated) holding through intent: the call's step
F_brel:                                                                      \* (mutated) its release
    Lock();                                                                  \* (mutated) it lets go by clearing intent
    intent := 0;                                                             \* with a locked store
    goto F_loop;                                                             \* and the call is finished
X_cas:                                                                       \* today's release: the compare-and-swap
    \* Today's release: one compare-and-swap from BUSY to zero; flags left meanwhile make it fail.
    Lock();                                                                  \* a locked instruction
    if ~Flagged then busy := FALSE; goto X_after; end if;                    \* no flags: the side is free
X_swap:                                                                      \* today's release: the swap of the flags
    Lock();                                                                  \* the flags, taken with one swap back to BUSY:
    sawReady := ready;                                                       \* READY,
    sawWaiting := waiting;                                                   \* WAITING,
    if retaking /\ waiting then releaseWoke := TRUE; end if;                 \* (record)
    if retaking /\ (defer \/ req) then retakeWork := TRUE; end if;           \* (record)
    ready := FALSE;                                                          \* all cleared, the side still held;
    waiting := FALSE;                                                        \* DEFER and REQ are done here and now,
    defer := FALSE;                                                          \* by the holder that took them
    req := FALSE;                                                            \* (their work is abstract)
X_serve:                                                                     \* today's release: READY
    if sawReady then dirty := FALSE; end if;                                 \* READY: the driver's part, done here
X_wake:                                                                      \* today's release: WAITING
    if sawWaiting then                                                       \* WAITING:
        owed := owed \cup contended;                                         \* the contended wakers are taken now,
        contended := {};                                                     \* to wake once the side is let go
    end if;                                                                  \* the flags are done
    goto X_cas;                                                              \* try again to let go
X_after:                                                                     \* today's release: let go
    woken := woken \cup owed;                                                \* let go: the taken wakers are woken
    owed := {};                                                              \* and forgotten
    retaking := FALSE;                                                       \* a release's retake is over too
    if role = "owner" then goto O_out;                                       \* the owner's edge is finished
    else goto F_loop;                                                        \* the foreign call is finished
    end if;                                                                  \* back to the caller's code
end process;                                                                 \* end of a thread
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* The places where a thread holds the side: biased, through today's word, or (mutated) through its intent alone.
BiasedHold == {"O_serve", "O_rel"}                                           \* the owner, biased
SharedHold == {"O_sserve", "F_step", "X_cas", "X_swap", "X_serve", "X_wake"} \* anyone, through today's word
IntentHold == {"F_bstep", "F_brel"}                                          \* a foreign thread, only under foreign_stays_biased
Holds(t) == pc[t] \in BiasedHold \cup SharedHold \cup IntentHold             \* thread `t` holds the side

\* At most one thread holds the side, and one that holds it through today's word has BUSY up.
Exclusive ==                                                                 \* holds when both parts do:
    /\ Cardinality({t \in Threads : Holds(t)}) <= 1                          \* one holder at most
    /\ \A t \in Threads : pc[t] \in SharedHold => busy                       \* and the word says so

\* No thread waits for another except by sleeping until it is woken, and none takes again without a new call or a wake.
NoSpin ==                                                                    \* holds when both parts do:
    /\ \A t \in Threads : pc[t] = "F_mode" => fresh[t]                       \* a take starts only a new call or after a wake
    /\ \A t \in Threads :                                                    \* and every thread that is not
        pc[t] \notin {"Done", "F_park", "S_wait", "O_out"}                   \* finished, asleep, a spare, or an owner
            => ENABLED thread(t)                                             \* waiting for work can always step

\* Once everything is finished, no work is left: every edge was served and no READY, WAITING, DEFER or REQ is up.
NoLostWork == Quiet => (~dirty /\ ~Flagged)                                  \* finished means every left work was done

\* Once memory says shared mode it says so for good, and no thread ever buffers a store of biased mode.
SharedAbsorbing ==                                                           \* over every step:
    [][ /\ (mem.mode = "shared" => mem'.mode = "shared")                     \* memory never goes back
        /\ \A t \in Threads : \A i \in DOMAIN buf'[t] :                      \* and no buffer
               buf'[t][i] # <<"mode", "biased">>                             \* holds a store of biased mode
      ]_vars                                                                 \* (a step that changes nothing passes)

\* `Termination` (every process ends, so every poll ran its step and no wake was lost) is the one
\* the translator writes for the fair processes.

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A foreign call sees own = 1 and leaves its work to the owner.
NoLeave == ~left                                                             \* never left
\* A foreign call switches the side to shared mode.
NoShared == ~switched                                                        \* never switched
\* The monitor moves the home shard to the spare.
NoHandoff == handoffs = 0                                                    \* never moved
\* The owner's release takes the side again and wakes a waiting poll.
NoReleaseWake == ~releaseWoke                                                \* never woken by a release's retake
\* The owner's release takes the side again and does a DEFER or a REQ a foreign call left.
NoRetakeWork == ~retakeWork                                                  \* never done by a release's retake
\* A foreign call's heavy fence fails with an owner claimed, and the owner switches the side to shared.
NoUnserved == ~(unservedSeen /\ switchedUnfenced)                            \* never served after a failed fence
\* A foreign call's heavy fence fails with no owner claimed, and the side goes shared.
NoFailShared == ~failShared                                                  \* never shared for a failed fence
\* The owner takes the side for a nudge.
NoNudgePass == ~nudgePass                                                    \* never nudged
=============================================================================

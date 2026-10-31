----------------------------- MODULE ShardLocal -----------------------------
(***************************************************************************)
(* A shard-local value lent to a task's code, while the monitor may hand   *)
(* the shard to another thread.                                            *)
(*                                                                          *)
(* The Rust it models: `ShardLocal::try_with` (src/shard_local.rs);        *)
(* `current_pinned` and `current`, whose bracket comes back into the shard *)
(* through `Here::come_back` and goes out again as it drops, and `poll`,   *)
(* which goes out at a poll's start and comes back at its end              *)
(* (src/rt/context.rs); `Slot::pin`, `unpin`, `is_pinned`, `go_out`,       *)
(* `come_back` with `contest`, `claim`, `take` and `cancel`                *)
(* (src/rt/handoff/slot.rs); the monitor's `take`, which claims, fences,   *)
(* reads the word again and the pins, then cancels or takes                *)
(* (src/rt/handoff/monitor.rs); and `Locals`, where the value lives, in    *)
(* the shard (src/rt/shard/locals.rs).                                     *)
(*                                                                          *)
(* In plain words. Who acts: thread A, which runs shard S with its slot;   *)
(* thread B, a spare; and the monitor. A runs S's loop and polls a task:   *)
(* it writes its slot's word "outside, epoch e" and leaves the shard to    *)
(* the task's code. That code calls `with`: A comes back into the shard    *)
(* (it writes "inside e", then reads its claim: none and it is in; a       *)
(* pending claim it cancels with a compare-and-swap; a taken one detaches *)
(* it, and `with` answers an access error and never touches the value),    *)
(* pins its slot, takes S's value, and may run a nested `block_on`, whose  *)
(* poll goes outside again (a new epoch) and comes back. Then it lets go   *)
(* of the value, unpins and goes outside again. The monitor, having seen   *)
(* the word outside, claims the slot (pending), runs its fence, reads the  *)
(* word again and then the pins: the word changed or the slot is pinned,   *)
(* and it cancels; otherwise it takes (pending to taken, a compare-and-    *)
(* swap a cancel by A beats) and hands S to B with a fresh slot. B runs    *)
(* S's task, whose code calls `with` too. A, once detached, finds no shard *)
(* in `current` and never gets the value again.                            *)
(*                                                                          *)
(* What must never happen: two threads holding the value at once           *)
(* (`ExclusiveValue`); a `with` on S getting any value but the one S holds *)
(* (`OneValuePerShard`); a thread whose shard was taken holding the value  *)
(* (`DetachedNeverHolds`).                                                 *)
(*                                                                          *)
(* What is abstract. Every step is one atomic load, store or compare-and-  *)
(* swap, sequentially consistent: the store buffer the heavy fence and the *)
(* runner's compiler fence exist for is `tla/handoff`'s (Handoff.tla), so  *)
(* here the fence orders nothing more. The pins are written by their       *)
(* runner alone, so a pin (a load and a store) is one step. The monitor    *)
(* takes after one look at an outside word, where the code waits for a     *)
(* streak of looks: every take the code makes is one the model makes. The *)
(* value is its identity: S's own (0), or (MUTATION per_thread) one per    *)
(* thread. The monitor hands S on once (B is the only spare). B's task     *)
(* makes one poll with one `with` and no nested `block_on`, as nobody      *)
(* watches B's slot after the handoff.                                     *)
(***************************************************************************)
EXTENDS Integers, FiniteSets  \* the standard modules used

CONSTANTS  \* what each configuration sets
    MaxPolls,   \* the polls of tasks thread A makes on S
    MaxWiths,   \* the calls of `with` A's task code makes, in all
    Nested,     \* whether A's `with` may run a nested `block_on`, whose poll goes outside S
    Mode        \* "ok", or the one rule this run breaks (see the configurations)

A == 1                          \* the thread that runs S first
B == 2                          \* the spare the monitor hands S to
Threads == {A, B}               \* both threads
Slots == {1, 2}                 \* slot 1 is A's; slot 2 the fresh one B gets with S
SlotOf(th) == th                \* each thread's slot
Owner(s) == s                   \* the thread each slot was made for
NONE == 0                       \* no claim: the shard is its runner's
PENDING == 1                    \* the monitor is about to take the shard
TAKEN == 2                      \* the monitor took the shard
Inside(e) == 2 * e              \* the word of a runner inside S in the poll of epoch e (`slot::inside`)
Outside(e) == 2 * e + 1         \* the word of a runner outside S in that poll (`slot::outside`)
IsOutside(w) == w % 2 = 1       \* the word says its runner is outside (`slot::is_outside`)
Polls(th) == IF th = A THEN MaxPolls ELSE 1  \* the polls each thread makes
Withs(th) == IF th = A THEN MaxWiths ELSE 1  \* the calls of `with` each thread's tasks make

\* The value a `with` by thread th gets: S's own, which lives in S (`Locals`).
\* MUTATION per_thread: the value is kept per thread (a thread-local), so each thread has its own.
Val(th) == IF Mode = "per_thread" THEN th ELSE 0  \* the value a with gets

VARIABLES  \* the state
    word,          \* each slot's word, which only its runner writes
    claim,         \* each slot's claim, which the monitor raises and either side settles
    pins,          \* each slot's pins: shard-local values lent to its runner's code now
    pc,            \* each thread's place in its program (see the steps below)
    ep,            \* each thread's epoch of the task's poll it is in
    ne,            \* each thread's epoch of the nested `block_on`'s poll inside its `with`
    epochs,        \* each thread's last epoch begun (`Here::epochs`)
    npolls,        \* the polls each thread began
    nwiths,        \* the calls of `with` each thread's task code began
    det,           \* each thread found S taken: it runs no shard from now on (`Here::detach`)
    ret,           \* where each thread's way back in goes once it read its claim
    holders,       \* the threads that hold the value now (inside the closure of `with`)
    seen,          \* every value a `with` on S got
    mpc,           \* the monitor's next step
    mv,            \* the word the monitor saw before its claim
    aDone,         \* (record) a `with` of A's let go of the value
    takeAfterWith, \* (record) the monitor took S after a `with` of A's had let go of it
    pinCancel      \* (record) the monitor cancelled because the slot was pinned

thrVars == <<pc, ep, ne, epochs, npolls, nwiths, det, ret>>  \* what each thread holds of its own
monVars == <<mpc, mv>>                                       \* what the monitor holds
recVars == <<aDone, takeAfterWith, pinCancel>>               \* what the run records
vars == <<word, claim, pins, thrVars, holders, seen, monVars, recVars>>  \* every variable

(***************************************************************************)
(* Helpers.                                                                *)
(***************************************************************************)

\* Thread th's word of its slot becomes w, unless th is detached: it has no slot any more.
Store(th, w) == word' = IF det[th] THEN word ELSE [word EXCEPT ![SlotOf(th)] = w]  \* th writes its word

\* Where a way back in goes when the shard is still th's: into `with`, after the nested poll,
\* or into the loop after the task's poll.
InTo(r) == CASE r = "with"   -> "w_pin"      \* `with` goes on: it pins next
             [] r = "nested" -> "w_back"     \* back in `with` from the nested poll
             [] OTHER        -> "loop"       \* the task's poll is over: the loop

\* Where it goes when the shard was taken: `with` answers its error to the task's code, the
\* nested poll's end goes back into the closure (which still runs), the task's poll's end
\* leaves the thread with no shard for good.
TakenTo(r) == CASE r = "with"   -> "task"    \* the access error: back in the task's code
                [] r = "nested" -> "w_back"  \* the closure goes on, the thread detached
                [] OTHER        -> "gone"    \* the thread runs S no more

(***************************************************************************)
(* A thread running S (A from the start, B once it is handed S).           *)
(***************************************************************************)

\* The loop polls a task (`poll`): a new epoch, the word outside, the task's code runs.
PollStart(th) ==  \* a poll begins
    /\ pc[th] = "loop"                                         \* th is in S's loop
    /\ npolls[th] < Polls(th)                                  \* a poll is left to make
    /\ npolls' = [npolls EXCEPT ![th] = @ + 1]                 \* one poll more
    /\ epochs' = [epochs EXCEPT ![th] = @ + 1]                 \* a new epoch
    /\ ep' = [ep EXCEPT ![th] = epochs[th] + 1]                \* the poll's epoch
    /\ Store(th, Outside(epochs[th] + 1))                      \* `go_out`: outside, in that epoch
    /\ pc' = [pc EXCEPT ![th] = "task"]                        \* the task's code runs
    /\ UNCHANGED <<claim, pins, ne, nwiths, det, ret, holders, seen, monVars, recVars>>  \* nothing else

\* The task's code calls `with` (`try_with`, `current_pinned`, `current`). A detached thread
\* finds no shard and gets the access error at once. Otherwise the bracket comes back in: the
\* word inside, then (next step) a look at the claim.
\* MUTATION no_bracket: `with` takes the value with no way back in: no store, no look.
WithBegin(th) ==  \* with is called
    /\ pc[th] = "task"                                         \* th runs the task's code
    /\ nwiths[th] < Withs(th)                                  \* a call of `with` is left
    /\ nwiths' = [nwiths EXCEPT ![th] = @ + 1]                 \* one call more
    /\ IF det[th]                                              \* no shard: `current` finds none
          THEN UNCHANGED <<word, pc, ret>>                     \* the error, back to the task's code
          ELSE IF Mode = "no_bracket"                          \* MUTATION no_bracket:
                  THEN /\ pc' = [pc EXCEPT ![th] = "w_pin"]    \* straight to the pin
                       /\ UNCHANGED <<word, ret>>              \* nothing written, nothing read
                  ELSE /\ Store(th, Inside(ep[th]))            \* `come_back`: the word inside
                       /\ ret' = [ret EXCEPT ![th] = "with"]   \* for `with`
                       /\ pc' = [pc EXCEPT ![th] = "cb_load"]  \* the look at the claim next
    /\ UNCHANGED <<claim, pins, ep, ne, epochs, npolls, det, holders, seen, monVars, recVars>>  \* nothing else

\* The task's poll ends (`poll`'s way back): the word inside, then a look at the claim. A thread
\* detached during the poll has no slot: it finds itself detached and leaves the shard for good.
PollEnd(th) ==  \* the poll ends
    /\ pc[th] = "task"                                         \* th runs the task's code
    /\ IF det[th]                                              \* detached during the poll:
          THEN /\ pc' = [pc EXCEPT ![th] = "gone"]             \* it runs S no more
               /\ UNCHANGED <<word, ret>>                      \* and writes nothing
          ELSE /\ Store(th, Inside(ep[th]))                    \* `come_back`: the word inside
               /\ ret' = [ret EXCEPT ![th] = "end"]            \* for the poll's end
               /\ pc' = [pc EXCEPT ![th] = "cb_load"]          \* the look at the claim next
    /\ UNCHANGED <<claim, pins, ep, ne, epochs, npolls, nwiths, det, holders, seen,  \* these keep their values:
                   monVars, recVars>>                          \* every variable not set above

\* `come_back`'s load of the claim, after the store of the word (and the compiler's fence):
\* none, and the thread is in.
CbLoad(th) ==  \* the look at the claim
    /\ pc[th] = "cb_load"                                      \* the word is stored
    /\ pc' = [pc EXCEPT ![th] = IF claim[SlotOf(th)] = NONE    \* no claim:
                                THEN InTo(ret[th])             \* in
                                ELSE "cb_cas"]                 \* a claim is up: `contest`
    /\ UNCHANGED <<word, claim, pins, ep, ne, epochs, npolls, nwiths, det, ret, holders, seen,  \* these keep their values:
                   monVars, recVars>>                          \* every variable not set above

\* `contest`: the compare-and-swap pending to none. It cancels a pending claim and goes in, goes
\* in if the monitor cancelled meanwhile, or finds the shard taken and detaches.
CbCas(th) ==  \* the contest
    /\ pc[th] = "cb_cas"                                       \* a claim was seen
    /\ IF claim[SlotOf(th)] = TAKEN                            \* the monitor took the shard:
          THEN /\ det' = [det EXCEPT ![th] = TRUE]             \* detached (`Back::Taken`)
               /\ pc' = [pc EXCEPT ![th] = TakenTo(ret[th])]   \* and out
               /\ UNCHANGED claim                              \* the swap fails
          ELSE /\ claim' = [claim EXCEPT ![SlotOf(th)] = NONE] \* pending (or none) to none
               /\ pc' = [pc EXCEPT ![th] = InTo(ret[th])]      \* in (`Back::In`)
               /\ UNCHANGED det                                \* still the shard's runner
    /\ UNCHANGED <<word, pins, ep, ne, epochs, npolls, nwiths, ret, holders, seen,  \* these keep their values:
                   monVars, recVars>>                          \* every variable not set above

\* `Slot::pin`, inside the shard: the slot is pinned while the value is lent.
Pin(th) ==  \* the pin
    /\ pc[th] = "w_pin"                                        \* in `with`, about to pin
    /\ pins' = [pins EXCEPT ![SlotOf(th)] = @ + 1]             \* one value lent more
    /\ pc' = [pc EXCEPT ![th] = "w_hold"]                      \* the value comes next
    /\ UNCHANGED <<word, claim, ep, ne, epochs, npolls, nwiths, det, ret, holders, seen,  \* these keep their values:
                   monVars, recVars>>                          \* every variable not set above

\* `Locals::get_or_init`: the closure gets the value and runs with it.
Hold(th) ==  \* the value lent
    /\ pc[th] = "w_hold"                                       \* pinned
    /\ holders' = holders \cup {th}                            \* th holds the value
    /\ seen' = seen \cup {Val(th)}                             \* the value it got
    /\ pc' = [pc EXCEPT ![th] = "w_in"]                        \* the closure runs
    /\ UNCHANGED <<word, claim, pins, ep, ne, epochs, npolls, nwiths, det, ret,  \* these keep their values:
                   monVars, recVars>>                          \* every variable not set above

\* The closure runs a nested `block_on`; its poll goes outside S in a new epoch (`poll`, `go_out`),
\* the value still held.
NestedOut(th) ==  \* a nested poll goes out
    /\ pc[th] = "w_in"                                         \* the closure holds the value
    /\ Nested                                                  \* this run nests a `block_on`
    /\ th = A                                                  \* in A's task
    /\ epochs' = [epochs EXCEPT ![th] = @ + 1]                 \* a new epoch
    /\ ne' = [ne EXCEPT ![th] = epochs[th] + 1]                \* the nested poll's epoch
    /\ Store(th, Outside(epochs[th] + 1))                      \* `go_out`: outside, in that epoch
    /\ pc' = [pc EXCEPT ![th] = "n_task"]                      \* the nested future's code runs
    /\ UNCHANGED <<claim, pins, ep, npolls, nwiths, det, ret, holders, seen,  \* these keep their values:
                   monVars, recVars>>                          \* every variable not set above

\* The nested poll ends (`poll`'s way back): the word inside, then a look at the claim.
NestedBack(th) ==  \* and comes back
    /\ pc[th] = "n_task"                                       \* in the nested poll
    /\ IF det[th]                                              \* no slot: no look
          THEN /\ pc' = [pc EXCEPT ![th] = "w_back"]           \* back in the closure
               /\ UNCHANGED <<word, ret>>                      \* writing nothing
          ELSE /\ Store(th, Inside(ne[th]))                    \* `come_back`: the word inside
               /\ ret' = [ret EXCEPT ![th] = "nested"]         \* for the nested poll
               /\ pc' = [pc EXCEPT ![th] = "cb_load"]          \* the look at the claim next
    /\ UNCHANGED <<claim, pins, ep, ne, epochs, npolls, nwiths, det, holders, seen,  \* these keep their values:
                   monVars, recVars>>                          \* every variable not set above

\* The closure returns: the value is let go of.
Release(th) ==  \* the value let go of
    /\ pc[th] \in {"w_in", "w_back"}                           \* the closure holds the value
    /\ holders' = holders \ {th}                               \* and lets go of it
    /\ aDone' = (aDone \/ th = A)                              \* (record) a `with` of A's ended
    /\ pc' = [pc EXCEPT ![th] = "w_unpin"]                     \* the unpin next
    /\ UNCHANGED <<word, claim, pins, ep, ne, epochs, npolls, nwiths, det, ret, seen,  \* these keep their values:
                   monVars, takeAfterWith, pinCancel>>         \* every variable not set above

\* `Pinned`'s drop: `Slot::unpin`, still inside the bracket.
Unpin(th) ==  \* the unpin
    /\ pc[th] = "w_unpin"                                      \* the value was let go of
    /\ pins' = [pins EXCEPT ![SlotOf(th)] = @ - 1]             \* one value lent less
    /\ pc' = [pc EXCEPT ![th] = "w_out"]                       \* the bracket's end next
    /\ UNCHANGED <<word, claim, ep, ne, epochs, npolls, nwiths, det, ret, holders, seen,  \* these keep their values:
                   monVars, recVars>>                          \* every variable not set above

\* `Bracket`'s drop: `go_out` in the task's poll's epoch, back to the task's code (a detached
\* thread has no slot, and writes nothing).
\* MUTATION no_bracket: there is no bracket, so nothing is written.
WithEnd(th) ==  \* the bracket ends
    /\ pc[th] = "w_out"                                        \* unpinned
    /\ IF Mode = "no_bracket"                                  \* MUTATION no_bracket:
          THEN UNCHANGED word                                  \* no bracket to drop
          ELSE Store(th, Outside(ep[th]))                      \* outside again, in the poll's epoch
    /\ pc' = [pc EXCEPT ![th] = "task"]                        \* the task's code goes on
    /\ UNCHANGED <<claim, pins, ep, ne, epochs, npolls, nwiths, det, ret, holders, seen,  \* these keep their values:
                   monVars, recVars>>                          \* every variable not set above

\* Every step of thread th.
Thread(th) ==  \* every step of th
    \/ PollStart(th) \/ WithBegin(th) \/ PollEnd(th)           \* the loop and the task's code
    \/ CbLoad(th) \/ CbCas(th)                                 \* a way back in
    \/ Pin(th) \/ Hold(th) \/ NestedOut(th) \/ NestedBack(th)  \* inside `with`
    \/ Release(th) \/ Unpin(th) \/ WithEnd(th)                 \* out of `with`

(***************************************************************************)
(* The monitor (`take` in src/rt/handoff/monitor.rs), watching A's slot.   *)
(***************************************************************************)

\* A spare waits, and the monitor sees A's word outside the shard: it keeps the word.
Watch ==  \* the monitor watches
    /\ mpc = "watch"                                           \* watching
    /\ pc[B] = "spare"                                         \* a spare to hand S to
    /\ IsOutside(word[1])                                      \* A is outside the shard
    /\ mv' = word[1]                                           \* the word it saw
    /\ mpc' = "claim"                                          \* it claims next
    /\ UNCHANGED <<word, claim, pins, thrVars, holders, seen, recVars>>  \* nothing else changes

\* `Slot::claim`: pending. Its heavy fence follows; at this grain it orders nothing more.
Claim ==  \* the claim
    /\ mpc = "claim"                                           \* about to claim
    /\ claim' = [claim EXCEPT ![1] = PENDING]                  \* the claim is pending
    /\ mpc' = "reword"                                         \* the look at the word next
    /\ UNCHANGED <<word, pins, thrVars, holders, seen, mv, recVars>>  \* nothing else changes

\* The word read again: changed, and A came back in meanwhile, so it cancels.
\* MUTATION no_recheck: the word is not read again; only the pins are.
Reword ==  \* the word read again
    /\ mpc = "reword"                                          \* after the fence
    /\ mpc' = IF word[1] # mv /\ Mode # "no_recheck"           \* the word changed:
              THEN "cancel"                                    \* cancel
              ELSE "pinread"                                   \* else the pins next
    /\ UNCHANGED <<word, claim, pins, thrVars, holders, seen, mv, recVars>>  \* nothing else changes

\* `Slot::is_pinned`: a value lent to A's code keeps the shard where it is, so it cancels.
\* MUTATION no_pin_check: the pins are not looked at.
PinRead ==  \* the pins read
    /\ mpc = "pinread"                                         \* the word was unchanged
    /\ IF pins[1] # 0 /\ Mode # "no_pin_check"                 \* the slot is pinned:
          THEN /\ mpc' = "cancel"                              \* cancel
               /\ pinCancel' = TRUE                            \* (record) for the pin
          ELSE /\ mpc' = "take"                                \* else the take next
               /\ UNCHANGED pinCancel                          \* nothing recorded
    /\ UNCHANGED <<word, claim, pins, thrVars, holders, seen, mv, aDone, takeAfterWith>>  \* nothing else

\* `Slot::cancel`: the compare-and-swap pending to none (a no-op if A cancelled first).
Cancel ==  \* the cancel
    /\ mpc = "cancel"                                          \* the monitor gives up
    /\ claim' = IF claim[1] = PENDING                          \* still pending:
                THEN [claim EXCEPT ![1] = NONE]                \* back to none
                ELSE claim                                     \* otherwise as it is
    /\ mpc' = "watch"                                          \* and watches again
    /\ UNCHANGED <<word, pins, thrVars, holders, seen, mv, recVars>>  \* nothing else changes

\* `Slot::take`: the compare-and-swap pending to taken; lost if A cancelled first.
Take ==  \* the take
    /\ mpc = "take"                                            \* about to take
    /\ IF claim[1] = PENDING                                   \* still pending:
          THEN /\ claim' = [claim EXCEPT ![1] = TAKEN]         \* taken
               /\ mpc' = "give"                                \* S goes to B next
               /\ takeAfterWith' = aDone                       \* (record) after a `with` of A's
          ELSE /\ mpc' = "watch"                               \* A came back first: watch again
               /\ UNCHANGED <<claim, takeAfterWith>>           \* nothing taken
    /\ UNCHANGED <<word, pins, thrVars, holders, seen, mv, aDone, pinCancel>>  \* nothing else

\* `give`: the spare gets S with a fresh slot (B's, in the loop, epoch 0), with every value in it.
Give ==  \* the gift
    /\ mpc = "give"                                            \* S is the monitor's
    /\ pc' = [pc EXCEPT ![B] = "loop"]                         \* B runs S's loop
    /\ mpc' = "done"                                           \* the one handoff is made
    /\ UNCHANGED <<word, claim, pins, ep, ne, epochs, npolls, nwiths, det, ret, holders, seen,  \* these keep their values:
                   mv, recVars>>                               \* every variable not set above

\* Every step of the monitor.
Monitor == Watch \/ Claim \/ Reword \/ PinRead \/ Cancel \/ Take \/ Give  \* the monitor's steps

(***************************************************************************)
(* The system.                                                             *)
(***************************************************************************)

Init ==  \* the start
    /\ word = [s \in Slots |-> Inside(0)]                      \* every runner in its loop, epoch 0
    /\ claim = [s \in Slots |-> NONE]                          \* no claim
    /\ pins = [s \in Slots |-> 0]                              \* no value lent
    /\ pc = [th \in Threads |-> IF th = A THEN "loop" ELSE "spare"]  \* A runs S, B is a spare
    /\ ep = [th \in Threads |-> 0]                             \* in no poll
    /\ ne = [th \in Threads |-> 0]                             \* in no nested poll
    /\ epochs = [th \in Threads |-> 0]                         \* no epoch begun
    /\ npolls = [th \in Threads |-> 0]                         \* no poll made
    /\ nwiths = [th \in Threads |-> 0]                         \* no `with` called
    /\ det = [th \in Threads |-> FALSE]                        \* nobody detached
    /\ ret = [th \in Threads |-> "end"]                        \* (set before each use)
    /\ holders = {}                                            \* nobody holds the value
    /\ seen = {}                                               \* no value got yet
    /\ mpc = "watch"                                           \* the monitor watches
    /\ mv = 0                                                  \* having seen nothing
    /\ aDone = FALSE                                           \* no `with` of A's ended
    /\ takeAfterWith = FALSE                                   \* no take yet
    /\ pinCancel = FALSE                                       \* no cancel for a pin yet

Next ==  \* every step
    \/ \E th \in Threads : Thread(th)                          \* a thread's step
    \/ Monitor                                                 \* the monitor's step

Spec == Init /\ [][Next]_vars                                  \* every behaviour from the start

(***************************************************************************)
(* What must hold.                                                         *)
(***************************************************************************)

\* At most one thread holds the value at any time.
ExclusiveValue == Cardinality(holders) <= 1  \* one holder at most

\* Every `with` on S, whichever thread runs it, gets one and the same value: S's own, before
\* and after the handoff.
OneValuePerShard == Cardinality(seen) <= 1  \* one value per shard

\* A thread whose shard was taken never holds the value from then on.
DetachedNeverHolds == \A s \in Slots : claim[s] = TAKEN => Owner(s) \notin holders  \* a detached thread holds nothing

\* Witnesses: each claims an interesting case never happens, and must be proved wrong.
NoTakeAfterWith == ~takeAfterWith       \* no handoff after a `with` let go of the value
NoPinCancel == ~pinCancel               \* no take cancelled because the slot was pinned
NoBHolds == B \notin holders            \* B never holds the value
=============================================================================

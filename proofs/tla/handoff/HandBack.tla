------------------------------ MODULE HandBack ------------------------------
(***************************************************************************)
(* The hand-back: a task whose poll lost its shard to the monitor, and     *)
(* that then calls into the shard (a `ShardLocal::with`), gets the shard   *)
(* back for that call from whichever thread runs it now, instead of an     *)
(* error. The design this checks, before any of its code is written:       *)
(*                                                                         *)
(* - The detached thread's call is where it starts: `current_pinned`'s     *)
(*   bracket (`Bracket::take`, `Slot::mark` in src/rt/handoff/slot.rs)     *)
(*   marks the thread's old slot and finds its claim taken, or the thread  *)
(*   already knows it is detached; either way it finds no shard            *)
(*   (src/rt/context.rs). A leaf push from the same thread                 *)
(*   (`push_lifo_marked`, `Slot::mark_unless_claimed`) backs out on that   *)
(*   claim to the full bracket, which finds no shard either and takes the  *)
(*   remote path: a push needs no hand-back.                               *)
(* - The call that needs the shard asks for it: a lend task, put into the  *)
(*   queues of the shard the thread was detached from (kept alive by       *)
(*   `run_shard`'s frame in src/rt/workers.rs while the poll runs), taken  *)
(*   by a draining shard too. Then it waits, its thread parked, running    *)
(*   nothing, until the shard is lent to it or the runtime shuts down.     *)
(* - A runner polls the lend task as any task, between its other polls     *)
(*   (`run_loop` in src/rt/shard/run.rs), in its worker loop or in a       *)
(*   nested `block_on`'s loop. The task's code makes a full bracket, so    *)
(*   the runner's word says inside for the whole loan and the monitor      *)
(*   takes nothing. With the bracket in, and no shard-local value lent on  *)
(*   this thread (its slot unpinned, the thread not itself in a loan), it  *)
(*   lends: a compare-and-swap from asked to lent; then the runner parks   *)
(*   until the shard is given back, ends its bracket and its poll. A       *)
(*   bracket that finds the shard taken puts the lend task back into the   *)
(*   shard's queues for its next runner. A value lent on this thread       *)
(*   defers the loan, the lend task still the shard's.                     *)
(* - The borrower, lent the shard, enters it (no slot: nobody watches it,  *)
(*   the runner's bracket keeps the monitor off), runs the call, leaves    *)
(*   and gives the shard back, by return or by unwind.                     *)
(* - The runtime shuts down: a waiting borrower is woken, and a            *)
(*   compare-and-swap from asked to gone settles it against a lend that    *)
(*   came first; its call then answers that the runtime is gone.           *)
(*                                                                         *)
(* In plain words. Who acts: up to three threads that each run the shard   *)
(* in turn (thread 1 first, the others spares the monitor hands it to);    *)
(* the monitor; a thread that pushes work to the shard; the runtime's      *)
(* shutdown; a worker stop that drains the shard. What can happen: a       *)
(* runner's poll goes outside the shard; the monitor takes the shard and   *)
(* hands it to a spare; the old runner, detached, goes on with its poll    *)
(* (its future's drop, after a cancel, included), whose calls find no      *)
(* shard; a `with` among them asks for the shard back and waits; the       *)
(* waiting task may be cancelled meanwhile; the closure on the lent shard  *)
(* may panic, or block for good; any runner may block in one poll for      *)
(* good, so the monitor takes the shard again; a runner may poll the lend  *)
(* task in a nested `block_on`'s loop, or in the loop of a nested          *)
(* `block_on` inside its own `with`; a borrower's closure may run such a   *)
(* loop too; a poll may lose the shard during a nested `block_on`'s poll;  *)
(* the worker may be stopped, or the runtime shut down, while a request    *)
(* waits. What must never happen: two actors inside the shard at once      *)
(* (`Exclusive`); two threads holding the shard's value at once            *)
(* (`ExclusiveValue`); a detached thread inside the shard, or holding its  *)
(* value, outside a loan made to it (`DetachedStaysOut`,                   *)
(* `DetachedHoldsOnlyLent`); a request lost, neither waiting in the        *)
(* shard's queues nor being served (`LendHeld`); a shard lent to a thread  *)
(* that is not in its loan (`LentHasBorrower`), or a runner waiting for a  *)
(* loan nobody holds (`LenderHasLoan`); a `with` answered with an error    *)
(* while the runtime runs (`NoFailedWith`, the failure this design         *)
(* removes). What must happen: every request ends, served or told the      *)
(* runtime is gone (`HandBackDone`), and every remote push runs            *)
(* (`Rescued`), whichever runner blocks for good.                          *)
(*                                                                         *)
(* What is abstract. Every load, store and compare-and-swap is one         *)
(* sequentially consistent step: the store buffer that the heavy fence and *)
(* the runner's compiler fence exist for is Handoff.tla's, which checks    *)
(* the bracket, the leaf push and the come-back of a poll under x86's      *)
(* store order. The hand-back adds no pair of a store and a later load of  *)
(* another word but the runtime's close against a waiter, which is         *)
(* `Notify`'s (proofs/tla/notify); its lend is a message passed with a     *)
(* release store and an acquire load. A poll makes at most `Calls` calls   *)
(* on the shard it runs, and its code, once detached, `DetachedCalls` more *)
(* on the slot it was taken from (Handoff.tla's D_unwind); a task's work   *)
(* is a step; a remote push runs in one step of the loop. A                *)
(* call that needs the shard but lends no value (a spawn onto it with no   *)
(* worker taking tasks) is a `with` without the value. The nested loop     *)
(* inside a `with` is the one step that polls a lend task while the value  *)
(* is lent (a value lent keeps the monitor off that loop's polls,          *)
(* ShardLocal.tla). A cancel changes only the task's state: its poll and   *)
(* its future's drop run as the poll's code. A thread's bound task is a    *)
(* borrower of its own (`BT`, thread 1's), run on its stack only when its  *)
(* wait runs tasks (a mutation). After its poll, a detached thread         *)
(* finishes its bound tasks, one of which may ask for the shard as its     *)
(* poll did (`BoundOf`, a request of its own), then it ends (in the code   *)
(* it waits as a spare); a spare that runs the shard is a fresh thread     *)
(* with a fresh slot.                                                      *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC                                           \* numbers, sets, TLC's helpers

CONSTANTS                                                                   \* what each configuration sets:
    NThreads,     \* the threads that run the shard in turn (thread 1 first)
    MaxPolls,     \* the polls of the shard's own tasks each runner makes per turn
    MaxHandoffs,  \* the handoffs the monitor may make
    Pushes,       \* the work other threads push to the shard
    Calls,        \* the calls into the runtime one poll makes on the shard it runs
    DetachedCalls,                                                           \* the calls a detached thread's code still makes, as Handoff.tla's D_unwind
    Withs,        \* the threads whose tasks' code may call `with`
    Nested,       \* a runner's `with` may run the shard's queues (a nested `block_on`)
    NestedLoan,   \* a borrower's `with` may run them too, on the lent shard
    NestedBlockOn,\* the threads whose tasks' code may call `block_on`, nesting the shard's loop
    BlockSecond,  \* the second runner's first poll of its own may never return
    Ends,         \* how a borrower's closure may end besides returning: {"panic", "block"}
    BoundWiths,   \* the threads whose bound tasks, finished after their poll lost the shard, call `with`
    Cancels,      \* a waiting borrower's task may be cancelled meanwhile
    Close,        \* the runtime may shut down
    Drain,        \* the worker may be stopped: it takes no new work and exits once drained
    Mode          \* "ok", or the one rule this run breaks (see the configurations)

Threads == 1..NThreads                \* the threads that run the shard, in turn
Mon == NThreads + 1                   \* the monitor
Pusher == NThreads + 2                \* a thread that pushes work to the shard
Closer == NThreads + 3                \* the runtime's shutdown
BT == NThreads + 4                    \* thread 1's bound task, run inside its wait (a mutation)
Drainer == NThreads + 5               \* the worker's stop
BoundOf(t) == 100 + t                 \* the request of thread t's bound task, after its poll
\* The request a call of thread t makes: its poll's ("task") or its bound task's ("bound").
Req(t, w) == IF w = "bound" THEN BoundOf(t) ELSE t
ThreadOf(u) == IF u > 100 THEN u - 100 ELSE u   \* the thread a request waits on
WretOf(u) == IF u > 100 THEN "bound" ELSE "task" \* whose call the request is
Borrowers == Threads \cup {BT} \cup {BoundOf(t) : t \in Threads}  \* every request, each its own
Slots == 1..(1 + MaxHandoffs)         \* slot 1 is thread 1's; each handoff makes a fresh one

NONE == 0                             \* no claim: the shard is its runner's
PENDING == 1                          \* the monitor is about to take the shard
TAKEN == 2                            \* the monitor took the shard

\* The steps of a borrower from its wait to the shard given back: where a lent borrower is.
LoanSteps == {"W_wait", "W_in", "W_back", "W_panic", "W_block", "W_nwait"}
\* The steps of a thread in no poll of the shard's tasks: not started, a spare, or gone.
Idle == {"T_begin", "Spare", "Gone", "Done"}

(* --algorithm HandBack
variables
    word = [s \in Slots |-> 0],       \* each slot's word: 0 inside (the loop, or a call), e > 0 outside in the poll of epoch e
    claim = [s \in Slots |-> NONE],   \* each slot's claim, which the monitor raises
    pins = [s \in Slots |-> 0],       \* each slot's pins: shard-local values lent to its runner's code
    inS = {1},                        \* the actors inside the shard: the only ones that touch its cells
    holders = {},                     \* the threads holding the shard's value (inside a `with`)
    runnerSlot = 1,                   \* the slot of the shard's runner, the one the monitor watches
    nextSlot = 2,                     \* the fresh slot the next handoff gives
    assign = [t \in Threads |-> 0],   \* the slot a spare is handed, with the shard
    gone = {},                                                               \* threads detached: they found the shard taken, and run no shard
    inbox = 0,                        \* remote pushes waiting in the shard's queues
    pushed = 0,                       \* remote pushes made
    ran = 0,                          \* remote pushes run
    lends = {},                       \* the borrowers whose lend task waits in the shard's queues
    lendOf = [t \in Threads |-> 0],   \* the borrower whose lend task thread t polls now, 0 for none
    loan = [u \in Borrowers |-> "none"],                                     \* each borrower's request: none, asked, lent, back or gone
    cancelled = [u \in Borrowers |-> FALSE], \* the waiting request's task was cancelled
    closed = FALSE,                   \* the runtime shut down
    draining = FALSE,                 \* the worker was stopped: it takes no new work
    handoffs = 0,                     \* the handoffs made
    failed = FALSE,                   \* (record) a `with` was answered with an error
    defers = 0,                       \* (record) a lend task was deferred: the value was lent
    lendTaken = 0,                    \* (record) a lend task's bracket found the shard taken
    staleBacks = 0,                                                          \* (record) a detached thread's leaf push backed out
    detachedCalls = 0,                \* (record) a call found its thread's shard taken
    nestedDetach = 0,                 \* (record) a poll lost the shard during a nested `block_on`'s poll
    nestedLoans = 0,                  \* (record) a nested `block_on`'s loop lent the shard
    casLost = 0,                      \* (record) a lend lost its compare-and-swap to a shutdown
    panics = 0,                       \* (record) a closure on a lent shard panicked
    drainLoans = 0,                   \* (record) a loan served while the worker drained
    boundLoans = 0,                   \* (record) a bound task's call ran on a lent shard
    loansServed = 0;                  \* (record) a borrower's call ran on the shard lent to it

define                                                                       \* the properties:
    \* At most one actor is inside the shard.
    Exclusive == Cardinality(inS) <= 1                                       \* one at most
    \* At most one thread holds the shard's value.
    ExclusiveValue == Cardinality(holders) <= 1                              \* one at most
    \* A request is never lost: its lend task waits in the shard's queues or is being polled.
    LendHeld == \A u \in Borrowers :
                    loan[u] = "asked" => (u \in lends \/ \E r \in Threads : lendOf[r] = u)
    \* No `with` is answered with an error while the runtime runs.
    NoFailedWith == ~failed                                                  \* never
    \* Every request ends: the call ran on the shard, or the runtime shut down.
    HandBackDone == \A u \in Borrowers \ {BT} : loan[u] = "asked" ~> loan[u] \in {"back", "gone"}
    \* Every remote push runs.
    Rescued == <>(ran = Pushes)                                              \* in the end
    \* The spare the monitor hands the shard to: the lowest-numbered (spares are alike).
    FirstSpare == CHOOSE u \in {v \in Threads : pc[v] = "Spare"} :
                      \A w \in {v \in Threads : pc[v] = "Spare"} : u <= w
    \* Witnesses: each claims an interesting case never happens, and must be proved wrong.
    NoLoan == loansServed = 0                                                \* no call ran on a lent shard
    NoDefer == defers = 0                                                    \* no lend task deferred
    NoLendTaken == lendTaken = 0                                             \* no lend task found the shard taken
    NoGone == \A u \in Borrowers : loan[u] # "gone"                          \* no borrower told of a shutdown
    NoStaleBack == staleBacks = 0                                            \* no stale leaf push backed out
    NoDetachedCall == detachedCalls = 0                                      \* no call found its shard taken
    NoNestedDetach == nestedDetach = 0                                       \* no shard lost in a nested poll
    NoNestedLoan == nestedLoans = 0                                          \* no loan from a nested loop
    NoCasLost == casLost = 0                                                 \* no lend lost to a shutdown
    NoPanicLoan == panics = 0                                                \* no closure panicked on a lent shard
    NoCancelWait == \A u \in Borrowers : ~(cancelled[u] /\ loan[u] = "back") \* no cancelled waiter served
    NoDrainLoan == drainLoans = 0                                            \* no loan while draining
    NoBoundLoan == boundLoans = 0                                            \* no bound task served
end define;

\* One more call of this thread's code: on the shard it runs, or detached.
macro CountCall() begin
    if self \in gone then
        dc := dc + 1;
    else
        calls := calls + 1;
    end if;
end macro;

\* Another thread pushes work to the shard (its `Shared`), whoever runs it; a stopped worker
\* takes no new work.
fair process pusher = Pusher
begin
P_push:
    while pushed < Pushes /\ ~closed /\ ~draining do                         \* until every push is made
        inbox := inbox + 1;                                                  \* one more waits
        pushed := pushed + 1;                                                \* one more made
    end while;
end process;

\* The runtime shuts down (`Monitor::close`, `stop_all`): the shard stops, and every waiter of
\* the runtime's close is woken. The lend tasks stay in the shard's queues, never polled again.
fair process closer = Closer
begin
C_close:
    await Close;                                                             \* only where the runtime may close
    closed := TRUE;                                                          \* shut down
end process;

\* The worker is stopped (`Workers::stop`, `Shared::drain`): it takes no new work, and its loop
\* ends once no task of it is live.
fair process drainer = Drainer
begin
D_drain:
    await Drain;                                                             \* only where the worker may stop
    draining := TRUE;                                                        \* draining
end process;

fair process thread \in Threads
\* sl: the slot this thread runs with; e: its last epoch begun; polls: its own polls this turn;
\* calls: the calls its poll made; dc: the calls its code made detached; saved: the word a call read
\* at its start, put back at its end; nsaved: the same for a nested `block_on`'s bracket; b: the
\* borrower whose lend task it polls; nret: where that lend task's poll returns.
\* nb: the polls of a nested `block_on`'s future; wret: whose call asks for the shard, the poll's
\* ("task") or, after the poll, a bound task's ("bound").
variables sl = 0, e = 0, polls = 0, calls = 0, dc = 0, saved = 0, nsaved = 0, b = 0, nret = "loop",
          nb = 0, wret = "task";
begin
T_begin:
    if self = 1 then                                                         \* thread 1 runs the shard first
        sl := 1;                                                             \* with slot 1
        goto R_loop;                                                         \* in its loop
    end if;
Spare:
    \* A spare waits to be handed the shard, or for the runtime to shut down.
    await assign[self] # 0 \/ closed;                                        \* handed it, or closed
    if assign[self] = 0 then                                                 \* closed, and never handed it:
        goto Gone;                                                           \* the spare goes
    end if;
T_take:
    sl := assign[self];                                                      \* its fresh slot
    assign[self] := 0;                                                       \* the gift taken
    polls := 0;                                                              \* none this turn
    inS := inS \cup {self};                                                  \* inside the shard, in its loop
R_loop:
    \* Inside the shard: its queues first (a lend task, then the pushed work), then a poll of its
    \* own, else asleep until work comes. A stopped runtime ends the loop; so does a drained worker
    \* (MUTATION drain_early: a detached thread's task, its poll still running, is not counted live).
    if closed then                                                           \* the runtime shut down:
        inS := inS \ {self};                                                 \* the runner leaves
        goto Gone;                                                           \* for good
    elsif draining /\ inbox = 0 /\ lends = {} /\ polls >= MaxPolls             \* drained, no other thread
          /\ (Mode = "drain_early" \/ \A t \in Threads \ {self} : pc[t] \in Idle) then  \* in a poll of its tasks:
        inS := inS \ {self};                                                 \* the worker exits
        goto Gone;                                                           \* for good
    elsif lends # {} then                                                    \* a lend task waits:
        with u \in lends do                                                  \* any of them
            b := u;                                                          \* its borrower
            lends := lends \ {u};                                            \* out of the queue
            lendOf[self] := u;                                               \* polled by this thread now
        end with;
        e := e + 1;                                                          \* its poll's epoch
        nret := "loop";                                                      \* polled by the worker loop
        goto N_poll;                                                         \* polled as any task
    elsif inbox > 0 then                                                     \* pushed work waits:
        inbox := inbox - 1;                                                  \* out of the queue
        ran := ran + 1;                                                      \* and run
        goto R_loop;                                                         \* the loop again
    elsif polls < MaxPolls then                                              \* a poll of its own is left:
        polls := polls + 1;                                                  \* one more
        e := e + 1;                                                          \* its epoch
    else                                                                     \* nothing to do:
        await closed \/ inbox > 0 \/ lends # {} \/ draining;                 \* asleep until work comes
        goto R_loop;                                                         \* then the loop again
    end if;
R_poll:
    \* `go_out`: the word says outside, in the poll's epoch; the task's code runs.
    word[sl] := e;                                                           \* outside, epoch e
    inS := inS \ {self};                                                     \* out of the shard
    calls := 0;                                                              \* no call made yet
R_task:
    \* The task's code (its future's poll, or its drop after a cancel): a call into the runtime (a
    \* full bracket, or the push of a task into the LIFO slot, tried first as a leaf), a `with`, a
    \* `block_on`, the poll's end, or (the second runner's first poll of its own, where allowed) a
    \* block that never returns. A detached thread's code goes on too, with `DetachedCalls` calls
    \* left, each on the slot it was taken from, as Handoff.tla's D_unwind; it asks for the shard at
    \* most once a poll (each request is a fresh loan in the code: a second is as the first).
    either
        await IF self \in gone THEN dc < DetachedCalls ELSE calls < Calls;   \* a call is left
        CountCall();                                                         \* one more
        goto B_store;                                                        \* a full bracket
    or
        await IF self \in gone THEN dc < DetachedCalls ELSE calls < Calls;   \* a call is left
        CountCall();                                                         \* one more
        goto L_store;                                                        \* the leaf push first
    or
        await self \in Withs /\ IF self \in gone                             \* a `with` may be called
                                 THEN dc < DetachedCalls /\ loan[Req(self, wret)] = "none"
                                 ELSE calls < Calls;
        CountCall();                                                         \* one more
        goto WB_store;                                                       \* `current_pinned`'s bracket
    or
        await calls < Calls /\ self \in NestedBlockOn /\ self \notin gone;   \* a `block_on` on the worker
        calls := calls + 1;                                                  \* one more
        goto NB_store;                                                       \* its bracket, then the loop
    or
        goto E_store;                                                        \* the poll ends
    or
        await BlockSecond /\ sl = 2 /\ polls = 1 /\ self \notin gone;        \* slot 2's first own poll
        goto Blocked;                                                        \* blocks for good
    end either;
B_store:
    \* A bracket (`Slot::mark`): it reads the word it left and marks the call (the word says
    \* inside). A detached thread marks its old slot, which nobody watches any more.
    saved := word[sl];                                                       \* the word it left
    word[sl] := 0;                                                           \* the mark
B_check:
    \* Its claim: taken, and the thread is detached (the call takes the remote paths: it needs no
    \* shard); pending, cancelled; none, in.
    if claim[sl] = TAKEN then                                                \* the shard was taken:
        gone := gone \cup {self};                                            \* detached
        detachedCalls := 1;                                                  \* (record)
        goto R_task;                                                         \* the call goes elsewhere
    else
        claim[sl] := NONE;                                                   \* a pending claim cancelled
        inS := inS \cup {self};                                              \* inside, for the call
    end if;
B_in:
    \* The call's work inside the shard; then it ends (`Slot::unmark`): the word it left, back.
    inS := inS \ {self};                                                     \* out again
    word[sl] := saved;                                                       \* outside, as before the call
    goto R_task;                                                             \* the task's code goes on
L_store:
    \* The leaf push (`Slot::mark_unless_claimed`): the mark, as a bracket's.
    saved := word[sl];                                                       \* the word it left
    word[sl] := 0;                                                           \* the mark
L_check:
    \* No claim: in, for the push. A claim up (a detached thread's old slot is taken for good):
    \* it never contests one, and backs out.
    if claim[sl] = NONE then                                                 \* no claim:
        inS := inS \cup {self};                                              \* inside
        goto L_in;                                                           \* for the push
    else                                                                     \* a claim:
        if self \in gone then                                                \* on the slot it was taken from:
            staleBacks := 1;                                                 \* (record)
        end if;
        goto L_back;                                                         \* backs out
    end if;
L_in:
    \* The push into the empty slot, or none (the slot holds a task, the task is another shard's).
    inS := inS \ {self};                                                     \* out at once
    either
        word[sl] := saved;                                                   \* pushed: the word back
        goto R_task;                                                         \* the call is done
    or
        goto L_back;                                                         \* not its push
    end either;
L_back:
    \* The word back, as before the leaf; then the full bracket.
    word[sl] := saved;                                                       \* the word it left
    goto B_store;                                                            \* the full bracket
WB_store:
    \* `with` (`ShardLocal::try_with`, `current_pinned`): first its bracket's mark.
    saved := word[sl];                                                       \* the word it left
    word[sl] := 0;                                                           \* the mark
WB_check:
    \* Taken: the thread is detached, and this is where the hand-back starts. Otherwise in.
    if claim[sl] = TAKEN then                                                \* the shard was taken:
        gone := gone \cup {self};                                            \* detached
        detachedCalls := 1;                                                  \* (record)
        goto W_ask;                                                          \* it asks for the shard back
    else
        claim[sl] := NONE;                                                   \* a pending claim cancelled
        inS := inS \cup {self};                                              \* inside
    end if;
WP_pin:
    \* `Slot::pin`, then the value lent to the closure.
    pins[sl] := pins[sl] + 1;                                                \* pinned
    holders := holders \cup {self};                                          \* holding the value
WP_in:
    \* The closure. It may run a nested `block_on`, whose loop polls a lend task waiting in the
    \* shard's queues: with its value lent, the runner defers the loan, the lend task still the
    \* shard's, to try again once the value is back (MUTATION lend_pinned: it lends; MUTATION
    \* defer_drops: the deferred lend task is lost).
    either
        await Nested /\ lends # {};                                          \* a lend task waits
        with u \in lends do                                                  \* this one
            b := u;                                                          \* its borrower
            if Mode = "lend_pinned" then                                     \* MUTATION lend_pinned:
                lends := lends \ {u};                                        \* taken out
                loan[u] := "lent";                                           \* the shard lent anyway
                inS := inS \ {self};                                         \* this thread parks
            elsif Mode = "defer_drops" then                                  \* MUTATION defer_drops:
                lends := lends \ {u};                                        \* the lend task lost
                defers := 1;                                                 \* (record)
            else                                                             \* the code:
                defers := 1;                                                 \* deferred, still queued
            end if;
        end with;
        if Mode = "lend_pinned" then                                         \* lent:
            goto WP_nwait;                                                   \* wait to get it back
        end if;
    or
        skip;                                                                \* no nested loop
    end either;
WP_release:
    \* The closure returns: the value let go of, then `Slot::unpin`.
    holders := holders \ {self};                                             \* the value back
    pins[sl] := pins[sl] - 1;                                                \* unpinned
WP_out:
    \* The bracket ends: the word it left, back.
    inS := inS \ {self};                                                     \* out
    word[sl] := saved;                                                       \* outside again
    goto R_task;                                                             \* the task's code goes on
WP_nwait:
    \* (MUTATION lend_pinned) The runner waits for the shard to come back.
    await loan[b] = "back";                                                  \* given back
    inS := inS \cup {self};                                                  \* inside again
    goto WP_release;                                                         \* the closure goes on
NB_store:
    \* `block_on` from a task on the worker: its bracket (`current`) marks the call, and the nested
    \* loop runs inside it.
    nsaved := word[sl];                                                      \* the word it left
    word[sl] := 0;                                                           \* the mark
    nb := 0;                                                                 \* no nested poll yet
NB_check:
    \* Taken: detached (its future runs on the thread's own shard, as the poll's code). Else in.
    if claim[sl] = TAKEN then                                                \* the shard was taken:
        gone := gone \cup {self};                                            \* detached
        detachedCalls := 1;                                                  \* (record)
        goto R_task;                                                         \* the code goes on, detached
    else
        claim[sl] := NONE;                                                   \* a pending claim cancelled
        inS := inS \cup {self};                                              \* inside, in the nested loop
    end if;
NB_loop:
    \* The nested loop, inside the shard: a lend task (lent from here: no value is lent on this
    \* thread, and its outer poll waits meanwhile), pushed work, one poll of the nested future, or
    \* the future done.
    either
        await lends # {};                                                    \* a lend task waits
        with u \in lends do                                                  \* this one
            b := u;                                                          \* its borrower
            lends := lends \ {u};                                            \* out of the queue
            lendOf[self] := u;                                               \* polled by this thread now
        end with;
        e := e + 1;                                                          \* its poll's epoch
        nret := "nested";                                                    \* polled by the nested loop
        goto N_poll;                                                         \* polled as any task
    or
        await inbox > 0;                                                     \* pushed work waits:
        inbox := inbox - 1;                                                  \* out of the queue
        ran := ran + 1;                                                      \* and run
        goto NB_loop;                                                        \* the nested loop goes on
    or
        await nb < 1;                                                        \* the future's poll:
        nb := nb + 1;                                                        \* one
        word[sl] := e + 1;                                                   \* outside,
        e := e + 1;                                                          \* in a new epoch
        inS := inS \ {self};                                                 \* out of the shard
        goto NB_back;                                                        \* and back at its end
    or
        goto NB_out;                                                         \* the future is done
    end either;
NB_back:
    \* A nested poll's end (`poll`'s come-back): the word inside.
    word[sl] := 0;                                                           \* inside
NB_bcheck:
    \* Taken during the nested poll: detached; the nested future goes to the thread's own shard,
    \* and the poll's code goes on, detached. Else back in the nested loop.
    if claim[sl] = TAKEN then                                                \* the shard was taken:
        gone := gone \cup {self};                                            \* detached
        nestedDetach := 1;                                                   \* (record)
        goto R_task;                                                         \* the code goes on, detached
    else
        claim[sl] := NONE;                                                   \* a pending claim cancelled
        inS := inS \cup {self};                                              \* inside
        goto NB_loop;                                                        \* the nested loop goes on
    end if;
NB_out:
    \* The nested future is done: `block_on`'s bracket ends, the outer poll's word back.
    inS := inS \ {self};                                                     \* out
    word[sl] := nsaved;                                                      \* outside again
    goto R_task;                                                             \* the task's code goes on
W_ask:
    \* The hand-back: the call puts a lend task into the shard's queues, which a draining shard
    \* takes too (MUTATION no_handback: the code before it, which answers the access error;
    \* MUTATION drain_refuses: a draining shard refuses it). Once the runtime is gone, gone at once.
    if Mode = "no_handback" \/ (Mode = "drain_refuses" /\ draining) then     \* the access error:
        failed := TRUE;                                                      \* answered so
        goto W_done;                                                         \* the code goes on
    elsif closed then                                                        \* the runtime shut down:
        loan[Req(self, wret)] := "gone";                                     \* the call answers so
        goto W_done;                                                         \* the code goes on
    else
        lends := lends \cup {Req(self, wret)};                               \* the lend task queued
        loan[Req(self, wret)] := "asked";                                    \* the request made
    end if;
W_wait:
    \* Parked, running nothing, until the shard is lent to it, or the runtime shuts down; then a
    \* compare-and-swap from asked to gone, lost to a lend that came first (MUTATION close_no_wake:
    \* it waits for the lend alone; MUTATION gone_no_cas: gone, a lend or not; MUTATION
    \* borrow_no_wait: in at once). Its task may be cancelled meanwhile: the wait goes on. (MUTATION
    \* wait_runs_tasks: the wait runs thread 1's bound task, which asks for the shard too.)
    either
        await loan[Req(self, wret)] = "lent" \/ Mode = "borrow_no_wait";     \* lent
        inS := inS \cup {self};                                              \* inside the shard
        holders := holders \cup {self};                                      \* the value lent to it
        if draining then                                                     \* while the worker drains:
            drainLoans := 1;                                                 \* (record)
        end if;
    or
        await closed /\ Mode # "close_no_wake"                               \* woken by the close,
              /\ (loan[Req(self, wret)] = "asked" \/ Mode = "gone_no_cas");  \* still asked:
        loan[Req(self, wret)] := "gone";                                     \* gone
        goto W_done;                                                         \* the call answers so
    or
        \* a cancel from elsewhere
        await Cancels /\ ~cancelled[Req(self, wret)] /\ loan[Req(self, wret)] = "asked";
        cancelled[Req(self, wret)] := TRUE;                                  \* the task's state changes
        goto W_wait;                                                         \* the wait goes on
    or
        \* MUTATION wait_runs_tasks:
        await Mode = "wait_runs_tasks" /\ self = 1 /\ wret = "task" /\ loan[BT] = "none";
        lends := lends \cup {BT};                                            \* the bound task asks too
        loan[BT] := "asked";                                                 \* from this thread's stack
        goto BT_wait;                                                        \* and waits, nested
    end either;
W_in:
    \* The closure, on the lent shard. A nested `block_on` in it may poll another borrower's lend
    \* task: a thread in a loan defers it (MUTATION lend_from_borrower: it lends it on). The
    \* closure may panic, or block for good.
    either
        await NestedLoan /\ lends # {};                                      \* a lend task waits
        with u \in lends do                                                  \* this one
            b := u;                                                          \* its borrower
            if Mode = "lend_from_borrower" then                              \* MUTATION lend_from_borrower:
                lends := lends \ {u};                                        \* taken out
                loan[u] := "lent";                                           \* the shard lent on
                inS := inS \ {self};                                         \* this thread parks
            else                                                             \* the code:
                defers := 1;                                                 \* deferred, still queued
            end if;
        end with;
        if Mode = "lend_from_borrower" then                                  \* lent on:
            goto W_nwait;                                                    \* wait to get it back
        end if;
    or
        await "panic" \in Ends;                                              \* the closure panics
        goto W_panic;                                                        \* and unwinds
    or
        await "block" \in Ends;                                              \* the closure blocks
        goto W_block;                                                        \* for good
    or
        skip;                                                                \* the closure returns
    end either;
W_back:
    \* The closure returns; the borrower leaves the shard and gives it back.
    holders := holders \ {self};                                             \* the value let go of
    inS := inS \ {self};                                                     \* out of the shard
    loan[Req(self, wret)] := "back";                                         \* given back
    loansServed := 1;                                                        \* (record)
    if wret = "bound" then                                                   \* a bound task's call:
        boundLoans := 1;                                                     \* (record)
    end if;
    goto W_done;                                                             \* the code goes on
W_panic:
    \* The unwind: the loan's guard leaves the shard and gives it back (MUTATION panic_keeps_loan:
    \* it leaves, and never gives it back). The panic ends the poll, caught by the task's run.
    holders := holders \ {self};                                             \* the value let go of
    inS := inS \ {self};                                                     \* out of the shard
    if Mode # "panic_keeps_loan" then                                        \* the guard:
        loan[Req(self, wret)] := "back";                                     \* given back
    end if;
    panics := 1;                                                             \* (record)
    if wret = "bound" then                                                   \* a bound task's poll:
        goto D_bound;                                                        \* it ends, the thread goes on
    else
        goto E_store;                                                        \* the poll ends
    end if;
W_done:
    \* The call is over: back to the code that made it, the poll's or a bound task's.
    if wret = "bound" then                                                   \* a bound task's:
        goto D_bound;                                                        \* the thread goes on
    else
        goto R_task;                                                         \* the poll's code goes on
    end if;
W_block:
    \* The closure never returns: the shard stays lent, as a pinned `with` holds it today.
    await FALSE;                                                             \* for good
W_nwait:
    \* (MUTATION lend_from_borrower) The borrower waits for the shard to come back.
    await loan[b] = "back";                                                  \* given back
    inS := inS \cup {self};                                                  \* inside again
    goto W_back;                                                             \* the closure goes on
BT_wait:
    \* (MUTATION wait_runs_tasks) The bound task's call waits on this thread, inside the wait of
    \* the thread's own call; lent, it runs and gives the shard back, and the outer wait goes on.
    await loan[BT] = "lent";                                                 \* lent to the bound task
    loan[BT] := "back";                                                      \* its call ran, given back
    goto W_wait;                                                             \* the outer wait goes on
N_poll:
    \* The lend task's poll, as any task's: outside, in a new epoch.
    word[sl] := e;                                                           \* outside, epoch e
    inS := inS \ {self};                                                     \* out of the shard
N_store:
    \* Its code's bracket: the mark (MUTATION lend_no_mark: none, so the word stays outside).
    saved := word[sl];                                                       \* the word it left
    if Mode # "lend_no_mark" then                                            \* the code:
        word[sl] := 0;                                                       \* the mark
    end if;
N_check:
    \* Its claim. Taken (MUTATION lend_no_check: not looked at): this thread is detached, and the
    \* lend task goes back into the shard's queues for its next runner (MUTATION lend_lost_on_take:
    \* it is dropped). Otherwise in.
    if claim[sl] = TAKEN /\ Mode # "lend_no_check" then                      \* the shard was taken:
        gone := gone \cup {self};                                            \* detached
        lendTaken := 1;                                                      \* (record)
        lendOf[self] := 0;                                                   \* polled no more
        if Mode # "lend_lost_on_take" then                                   \* the code:
            lends := lends \cup {b};                                         \* back in the queue
        end if;
        if nret = "nested" then                                              \* in a nested loop:
            goto R_task;                                                     \* the outer code, detached
        else
            goto E_store;                                                    \* the poll ends
        end if;
    else
        if claim[sl] = PENDING then                                          \* a pending claim:
            claim[sl] := NONE;                                               \* cancelled
        end if;
        inS := inS \cup {self};                                              \* inside
    end if;
N_lend:
    \* The loan: a compare-and-swap from asked to lent (MUTATION lend_no_cas: lent over gone). Lost
    \* to a shutdown, it lends nothing. Lent, the shard is the borrower's, and this thread touches
    \* nothing of it, its bracket keeping the monitor off (MUTATION lend_no_wait: it stays inside).
    lendOf[self] := 0;                                                       \* polled no more
    if loan[b] = "asked" \/ Mode = "lend_no_cas" then                        \* still asked:
        loan[b] := "lent";                                                   \* lent
        if nret = "nested" then                                              \* from a nested loop:
            nestedLoans := 1;                                                \* (record)
        end if;
        if Mode # "lend_no_wait" then                                        \* the code:
            inS := inS \ {self};                                             \* parked, outside the cells
        end if;
    else                                                                     \* gone first:
        casLost := 1;                                                        \* (record)
        goto N_unmark;                                                       \* nothing lent
    end if;
N_wait:
    \* Parked until the shard is given back.
    await loan[b] = "back" \/ Mode = "lend_no_wait";                         \* given back
    if Mode # "lend_no_wait" then                                            \* the code:
        inS := inS \cup {self};                                              \* inside again
    end if;
N_unmark:
    \* The bracket ends, then the poll.
    inS := inS \ {self};                                                     \* out
    word[sl] := saved;                                                       \* the poll's word back
    if nret = "nested" then                                                  \* in a nested loop:
        goto NB_back;                                                        \* its come-back
    else
        goto E_store;                                                        \* the poll ends
    end if;
E_store:
    \* The poll ends (`come_back_from_poll`): the word inside.
    word[sl] := 0;                                                           \* inside
E_check:
    \* Its claim: taken, and the thread is detached for good; else back in its loop.
    if claim[sl] = TAKEN then                                                \* the shard was taken:
        gone := gone \cup {self};                                            \* detached
        goto D_bound;                                                        \* it runs no shard
    else
        claim[sl] := NONE;                                                   \* a pending claim cancelled
        inS := inS \cup {self};                                              \* inside
        goto R_loop;                                                         \* the loop again
    end if;
Blocked:
    await FALSE;                                                             \* never returns
D_bound:
    \* A detached thread finishes its bound tasks on its own shard (`finish_home`): one of them may
    \* call `with`, which asks for the shard back as the poll's call did (the shard counts the bound
    \* task among its live ones, so a stopped worker waits for it).
    if self \in BoundWiths /\ wret = "task" then                             \* a bound task's `with`:
        wret := "bound";                                                     \* its call
        goto W_ask;                                                          \* asks for the shard
    end if;
Gone:
    skip;                                                                    \* this thread runs the shard no more
end process;

\* The monitor: sees the runner's word outside the shard, claims, reads the word and the pins again,
\* and takes (or cancels), then hands the shard to a spare with a fresh slot (`take` and `give` in
\* src/rt/handoff/monitor.rs). Each read is a tick apart or more in the code; any outside word is
\* one it may take here. A spare is always there to start (the code starts one when none waits).
fair process monitor = Mon
variables ms = 0, mv = 0;                                                    \* the slot it watches; the word it saw
begin
M_tick:
    while handoffs < MaxHandoffs do                                          \* while a handoff is left
        await closed \/ (word[runnerSlot] > 0 /\ \E t \in Threads : pc[t] = "Spare");  \* outside, a spare
        if closed then                                                       \* the runtime shut down:
            goto M_done;                                                     \* the watch ends
        end if;
M_look:
        ms := runnerSlot;                                                    \* the runner's slot
        mv := word[runnerSlot];                                              \* the word seen
M_claim:
        claim[ms] := PENDING;                                                \* the claim
M_check:
        \* The word again, then the pins: changed or pinned, and it cancels.
        if word[ms] # mv \/ pins[ms] # 0 then                                \* back in, or a value lent:
            claim[ms] := NONE;                                               \* cancelled
            goto M_tick;                                                     \* watch again
        end if;
M_take:
        \* Pending to taken, unless the runner cancelled first.
        if claim[ms] = PENDING then                                          \* still pending:
            claim[ms] := TAKEN;                                              \* taken
            inS := inS \cup {Mon};                                           \* the shard is the monitor's
        else
            goto M_tick;                                                     \* the runner came back first
        end if;
M_give:
        \* To a spare, with a fresh slot; with none left (the runtime shut down), the shard goes.
        if \E u \in Threads : pc[u] = "Spare" then                           \* a spare waits:
            assign[FirstSpare] := nextSlot;                                  \* it gets the shard
            runnerSlot := nextSlot;                                          \* watched from now on
            nextSlot := nextSlot + 1;                                        \* the next fresh slot
            inS := inS \ {Mon};                                              \* the monitor lets go
            handoffs := handoffs + 1;                                        \* one more handoff
        else
            inS := inS \ {Mon};                                              \* the shard goes
            goto M_done;                                                     \* the watch ends
        end if;
    end while;
M_done:
    skip;                                                                    \* no more handoffs
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

\* A runner waiting for the shard back waits on a loan that was made. (Here, past the translation,
\* since it reads each thread's own variables.)
LenderHasLoan == \A r \in Threads : pc[r] = "N_wait" => loan[b[r]] \in {"lent", "back"}

\* A detached thread is inside the shard only while the shard is lent to its call.
DetachedStaysOut ==
    \A t \in Threads : (t \in gone /\ t \in inS) => loan[Req(t, wret[t])] = "lent"

\* A detached thread holds the value only while the shard is lent to its call.
DetachedHoldsOnlyLent ==
    \A t \in Threads : (t \in gone /\ t \in holders) => loan[Req(t, wret[t])] = "lent"

\* A shard lent to a request finds its call in its loan, on its thread.
LentHasBorrower ==
    \A u \in Borrowers \ {BT} :
        loan[u] = "lent" => (pc[ThreadOf(u)] \in LoanSteps /\ wret[ThreadOf(u)] = WretOf(u))
=============================================================================

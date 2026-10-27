--------------------------- MODULE BoundedChannel ---------------------------
(***************************************************************************)
(* The bounded channel of nuclei::sync::mpsc (src/sync/mpsc/ and           *)
(* src/sync/fair_semaphore.rs), at the grain of its atomic steps.          *)
(*                                                                          *)
(* In plain words: a channel has Cap slots. A sender takes slots (permits)  *)
(* from a counter before it puts values in the message queue, and the      *)
(* receiver gives a slot back for every value it takes out. When no slot   *)
(* is free a sender waits in line. The line is served strictly first come, *)
(* first served: free slots are set aside for the oldest waiter until it   *)
(* has all it asked for. A waiter may give up (its future is dropped); then *)
(* whatever was set aside or handed to it goes on to the next one. The     *)
(* receiver can close the channel (no new sends) or drop it.               *)
(*                                                                          *)
(* What is modelled step by step: the semaphore's state word (free count,  *)
(* the closed bit, the QUEUED bit and the three bits of the drain role),   *)
(* the arrival stack, the waiting list and its set-aside, every waiter     *)
(* node (its state and whether its waker is armed), the drain pass that    *)
(* grants waiters, the waker handshake between a grant and a waiter that   *)
(* parks, cancellation, permits, the receiver's recv, close and drop, and  *)
(* the receiver's own register-then-look-again handshake.                  *)
(*                                                                          *)
(* The message queue is a FIFO sequence of cells in the order sends       *)
(* claimed them. A send claims its cell in one step and writes its value   *)
(* in a later one, as in the channel's segment queue, so the receiver can  *)
(* find the oldest cell claimed and not written yet. The queue answers     *)
(* that nothing is there, as for an empty queue, so the receiver reads the *)
(* sender count, which the send still writing keeps above zero, takes      *)
(* nothing and waits for that send's wake.                                 *)
(*                                                                          *)
(* What is abstract: the queue's lock-free implementation, checked in     *)
(* `segments` (its claim order is this sequence's order); the              *)
(* receiver's waker cell is one flag that a wake empties (its handshake is *)
(* checked by the loom models); a pass's look at the head of the waiting   *)
(* list, which skips withdrawn waiters, is one step (a waiter never comes  *)
(* back from withdrawn, so this is one of the orders the real loads can    *)
(* take); and the receiver's look at the queue, the sender count and the   *)
(* queue again (`Chan::pop`) is one step too, as the queue is abstract     *)
(* anyway.                                                                 *)
(*                                                                          *)
(* `Mutation` names one rule taken away, so that a configuration can show *)
(* the property that rule protects really is checked (see README).         *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    Cap,       \* how many slots the channel has (the `buffer` of `channel(buffer)`)
    Senders,   \* the sender threads, numbered 1, 2, ...
    Prog,      \* what each sender does, in order: a sequence of operations (see below)
    RxProg,    \* what the receiver does, in order: "recv", "drain", "close" or "drop"
    Mutation   \* "none", or the name of the one rule this run takes away

(* An operation of a sender is a record [op |-> o, n |-> k, use |-> u]:    *)
(*   o = "reserve": wait for k slots (`reserve_many(k)`, or `send` when    *)
(*       k = 1), put u values in through them, give the other k - u back;  *)
(*   o = "try": the same with `try_reserve_many(k)`, which never waits;    *)
(*   o = "cancel": start `reserve_many(k)`, poll it once, and drop it      *)
(*       (if it was granted by then, its slots are given straight back).   *)

\* The receiver's thread number: no sender uses 0.
Rx == 0
\* Every thread of the model.
Procs == Senders \cup {Rx}
\* One request per operation of a sender: <<sender, position in its program>>.
Reqs == UNION {{<<s, j>> : j \in 1..Len(Prog[s])} : s \in Senders}
\* The value of a request variable that holds no request yet.
NoReq == <<0, 0>>
\* The operation a request stands for.
OpOf(r) == Prog[r[1]][r[2]]
\* The smallest number of a non-empty set.
Min(S) == CHOOSE x \in S : \A y \in S : x <= y
\* The smaller of two numbers.
Min2(a, b) == IF a < b THEN a ELSE b
\* The sum of f[x] over every x of the finite set D.
RECURSIVE SumOver(_, _)
SumOver(f, D) == IF D = {} THEN 0
                 ELSE LET x == CHOOSE y \in D : TRUE IN f[x] + SumOver(f, D \ {x})
\* The set of entries of a sequence.
Range(s) == {s[j] : j \in DOMAIN s}

(* --algorithm BoundedChannel
variables
    \* ---- the semaphore's state word (one atomic word in the code) ----
    free = Cap,           \* slots free in the word: what `capacity()` reports
    closedBit = FALSE,    \* CLOSED: the receiver closed (or dropped) the channel
    queuedBit = FALSE,    \* QUEUED: a waiter may be queued, so attempts that do not queue must fail
    draining = FALSE,     \* DRAINING: some thread holds the drain role (serves the queue)
    dirty = FALSE,        \* DIRTY: the queue changed while the role was held: pass again
    wakeFront = FALSE,    \* WAKE_FRONT: a release came while the role was held: pass again
    \* ---- the queue (`WaitQueue` and the semaphore's `Waiting`) ----
    arrivals = <<>>,      \* the arrival stack, oldest first: nodes pushed and not yet sorted
    list = <<>>,          \* the waiting list, oldest first; only the drain role touches it
    setAside = 0,         \* slots taken out of the word for the front waiter, not yet granted
    \* ---- one waiter node per request ----
    nodeState = [r \in Reqs |-> "none"], \* "none", then "waiting", then "granted" or "cancelled"
    armed = [r \in Reqs |-> FALSE],     \* the node holds a waker nobody has taken yet
    closedG = [r \in Reqs |-> FALSE],   \* the grant was the close (CLOSED_GRANT): it carries nothing
    need = [r \in Reqs |-> 0],          \* the node's payload: how many slots it asks for
    owned = [r \in Reqs |-> 0],         \* slots a grant handed to the node, not yet taken by its future
    woken = [r \in Reqs |-> FALSE],     \* the waker of the request's task was called
    \* ---- the channel ----
    held = [s \in Senders |-> 0],       \* slots a sender holds as permits
    inHand = [p \in Procs |-> 0],       \* slots a thread is in the middle of giving back
    mq = <<>>,                          \* the message queue: values in the order their cells were claimed
    unready = {},                       \* values whose cell is claimed but not written yet
    txCount = Cardinality(Senders),     \* how many senders are alive
    rxRegistered = FALSE,               \* the receiver's waker cell holds its waker
    rxWoken = FALSE,                    \* the receiver's waker was called
    \* ---- records the properties read; the code has no such variables ----
    status = [r \in Reqs |-> "new"],    \* how far each request got (see Status below)
    ahead = [r \in Reqs |-> {}],        \* the requests already waiting when this one began
    lateStart = [r \in Reqs |-> FALSE], \* this request began after the close
    admittedAfterClose = FALSE,         \* slots left the word after the close
    pushed = {},                        \* every value ever put into the queue
    received = <<>>,                    \* every value the receiver took, in order
    dropped = {},                       \* every value the receiver's drop threw away
    rxEnded = FALSE,                    \* a receive returned "the channel has ended"
    pushAfterEnd = FALSE;               \* a value went in after a receive returned the end

define
    \* The receiver may report the end of a closed channel only once no slot is out anywhere.
    Finished == closedBit /\ free = Cap
    \* The oldest cell of the queue holds its value: the receiver can take it.
    FrontReady == mq # <<>> /\ Head(mq) \notin unready
    \* The oldest cell is claimed and not written yet: the queue answers `Busy`.
    FrontBusy == mq # <<>> /\ Head(mq) \in unready
    \* For the "skip_busy" mutation: the position of the first written cell after the
    \* oldest one, or 0 when there is none.
    FirstWritten == LET ws == {j \in 2..Len(mq) : mq[j] \notin unready}
                    IN IF ws = {} THEN 0 ELSE Min(ws)
    \* The waiting list after the pass let go of the withdrawn waiters at its head.
    PruneFront(l) == LET live == {j \in 1..Len(l) : nodeState[l[j]] # "cancelled"}
                     IN IF live = {} THEN <<>> ELSE SubSeq(l, Min(live), Len(l))
    \* For the "skip_unfit_front" mutation: the position of the first waiter after the head
    \* that is still waiting and asks for at most n slots, or 0 when there is none.
    FirstFit(l, n) == LET fits == {j \in 2..Len(l) : nodeState[l[j]] = "waiting" /\ need[l[j]] <= n}
                      IN IF fits = {} THEN 0 ELSE Min(fits)
    \* The sequence l without its j-th entry.
    RemoveAt(l, j) == SubSeq(l, 1, j - 1) \o SubSeq(l, j + 1, Len(l))
end define;

\* The receiver takes the oldest value out of the queue; its slot is in the receiver's hand
\* until the release that follows.
macro take_value() begin
    \* MUTATION queue_lifo: the queue hands out its newest value instead.
    received := Append(received, IF Mutation = "queue_lifo" THEN mq[Len(mq)] ELSE Head(mq));
    \* MUTATION double_delivery: the value is read but left in the queue.
    mq := IF Mutation = "double_delivery" THEN mq
          ELSE IF Mutation = "queue_lifo" THEN SubSeq(mq, 1, Len(mq) - 1) ELSE Tail(mq);
    inHand[Rx] := 1;                        \* the value's slot, on its way back to the word
end macro;

\* Wakes the receiver: the waker cell gives its waker up and calls it (`AtomicWaker::wake`).
macro wake_rx() begin
    if rxRegistered then
        rxRegistered := FALSE;   \* the cell is empty again
        rxWoken := TRUE;         \* and the receiver's task is told
    end if;
end macro;

(***************************************************************************)
(* The drain role's work (`Policy::drain` and `FairSemaphore::pass`): pass *)
(* over the queue until a pass ends with nothing new, then give the role   *)
(* up and call the wakers of the waiters granted meanwhile.                *)
(***************************************************************************)
procedure Drain()
variables
    found = FALSE,     \* the last pass left somebody waiting
    wl = {},           \* the wake list: requests whose waker this drain claimed
    front = NoReq;     \* the waiter the pass looks at
begin
D_clear:
    \* One fetch_and: this pass answers every DIRTY and WAKE_FRONT request so far.
    dirty := FALSE;
    wakeFront := FALSE;
D_absorb:
    \* One swap takes the whole arrival stack; its nodes join the list, oldest first.
    list := list \o arrivals;
    arrivals := <<>>;
D_closed:
    \* One load of the state word: a closed semaphore fails every waiter.
    if closedBit then goto D_fail; else goto D_front; end if;
D_fail:
    \* Closed: grant every waiter the close, oldest first, one node per step.
    if list = <<>> then
        goto D_return;                      \* nobody is left: give the set-aside back
    else
        front := Head(list);                \* the oldest node
        list := Tail(list);                 \* leaves the list
        if nodeState[front] = "waiting" then    \* a node that withdrew is simply let go
            nodeState[front] := "granted";      \* the grant ...
            closedG[front] := TRUE;         \* ... is the close
            status[front] := "failed";      \* this request ends in an error
            \* MUTATION grant_no_wake: the grant leaves the armed waker where it is.
            if armed[front] /\ Mutation # "grant_no_wake" then   \* an armed waker is claimed
                armed[front] := FALSE;      \* taken off the node ...
                wl := wl \cup {front};      \* ... onto the wake list
            end if;
        end if;
        goto D_fail;                        \* next node
    end if;
D_front:
    \* `front()`: let go of withdrawn waiters at the head, then look at the oldest one left.
    list := PruneFront(list);
    if list = <<>> then
        goto D_return;                      \* nobody waits: give the set-aside back
    else
        front := Head(list);                \* the oldest waiter still waiting
    end if;
D_take:
    \* `take_up_to`: one compare-and-swap moves free slots from the word to the set-aside,
    \* at most what the front still lacks, and none once the semaphore is closed.
    \* MUTATION take_after_close: slots are set aside after the close too.
    if setAside < need[front] /\ (~closedBit \/ Mutation = "take_after_close") then
        with t = Min2(free, need[front] - setAside) do
            free := free - t;               \* the slots leave the word ...
            setAside := setAside + t;       \* ... and wait for the front
            if closedBit /\ t > 0 then      \* only the mutation takes slots after the close
                admittedAfterClose := TRUE;
            end if;
        end with;
    end if;
D_covered:
    \* Is the front's whole request set aside? Then it is granted next.
    if setAside >= need[front] then
        list := Tail(list);                 \* it leaves the list; the grant follows
        goto D_grant;
    elsif Mutation = "skip_unfit_front" then
        goto D_skip;                        \* the mutation serves someone behind instead
    else
        found := TRUE;                      \* the front keeps its share and waits for more
        goto D_exit;
    end if;
D_skip:
    \* MUTATION skip_unfit_front: all or nothing. The front gives its share back, and the
    \* first waiter behind it whose request fits the free slots is served ahead of it.
    with j = FirstFit(list, free + setAside) do
        if j = 0 then
            free := free + setAside;        \* nobody fits: the share goes back, all wait
            setAside := 0;                  \* nothing is set aside any more
            found := TRUE;                  \* somebody still waits
            goto D_exit;
        else
            front := list[j];               \* the later, smaller request ...
            list := RemoveAt(list, j);      \* ... leaves the list ...
            free := free + setAside - need[front];   \* ... with the slots it asks for
            setAside := need[front];                 \* set aside for its grant
            goto D_grant;
        end if;
    end with;
D_grant:
    \* `WakeList::grant`: one compare-and-swap on the node, which only a waiting node takes.
    if nodeState[front] = "waiting" then
        nodeState[front] := "granted";          \* the node is granted ...
        owned[front] := need[front];        \* ... and carries its slots to its future
        setAside := setAside - need[front]; \* which leave the set-aside
        status[front] := "granted";         \* (record) this request has its slots
        \* MUTATION grant_no_wake: the grant leaves the armed waker where it is.
        if armed[front] /\ Mutation # "grant_no_wake" then   \* an armed waker is claimed
            armed[front] := FALSE;          \* taken off the node ...
            wl := wl \cup {front};          \* ... onto the wake list
        end if;
    elsif Mutation = "drop_pass_on" then
        \* MUTATION drop_pass_on: the share of a waiter that withdrew at its grant is lost.
        setAside := setAside - need[front];
    end if;
    \* Otherwise it withdrew as it was granted: its share stays set aside for the next waiter.
    goto D_front;
D_return:
    \* `return_set_aside`: with nobody waiting (or closed) the set-aside goes back to the
    \* word, in one fetch_add.
    if Mutation = "drop_pass_on" then
        skip;                               \* MUTATION drop_pass_on: the set-aside is lost
    elsif Mutation = "close_keeps_set_aside" /\ closedBit then
        skip;                               \* MUTATION close_keeps_set_aside: the close keeps it
    else
        free := free + setAside;            \* the set-aside is free again ...
        setAside := 0;                      \* ... and nothing is set aside
    end if;
    found := FALSE;                         \* this pass found nobody waiting
D_exit:
    \* The compare-and-swap that gives the role up: it fails, and the drain passes again,
    \* when the queue changed meanwhile, or when a release asked and somebody waits.
    \* MUTATION exit_ignores_release: the role is given up over a release's request.
    if dirty \/ (found /\ wakeFront /\ Mutation # "exit_ignores_release") then
        goto D_clear;
    else
        draining := FALSE;                  \* the role is free again
        if ~found then                      \* nobody waits: QUEUED and WAKE_FRONT come down
            queuedBit := FALSE;             \* attempts that do not queue may take slots again
            wakeFront := FALSE;             \* a release's request is void with nobody to serve
        end if;
    end if;
D_wake:
    \* With the role given up, the claimed wakers are called (the wake list's drop).
    woken := [r \in Reqs |-> woken[r] \/ r \in wl];
    return;                                 \* the wake list ends with the drain
end procedure;

(***************************************************************************)
(* `FairSemaphore::release(back)`: `back` slots come back to the word;    *)
(* when a waiter may be queued, ask for a pass.                           *)
(***************************************************************************)
procedure Release(back)
variables wasQueued = FALSE;   \* the word the fetch_add saw had QUEUED up
begin
P_add:
    \* One fetch_add: the slots are back in the word.
    wasQueued := queuedBit;                 \* the word as the fetch_add saw it
    free := free + back;                    \* the slots are free ...
    inHand[self] := inHand[self] - back;    \* ... and no longer in this thread's hand
P_ask:
    \* `Step::Wake`: take the role and pass, or leave the request with the role's holder.
    \* MUTATION release_no_pass: a release never asks for a pass.
    if wasQueued /\ Mutation # "release_no_pass" then
        if draining then
            wakeFront := TRUE;              \* the holder passes again for it
            return;
        else
            draining := TRUE;               \* this thread serves the queue itself
            call Drain();
            return;
        end if;
    else
        return;                             \* nobody waits: nothing more to do
    end if;
end procedure;

(***************************************************************************)
(* The receiver's close (`close_step`): set CLOSED with a draining         *)
(* transition, so the pass that sees the bit fails every waiter.           *)
(***************************************************************************)
procedure Close()
begin
K_close:
    if closedBit then
        return;                             \* closing twice does nothing
    else
        closedBit := TRUE;                  \* one compare-and-swap sets the bit ...
        if draining then
            dirty := TRUE;                  \* ... and marks the role's holder dirty
            return;
        else
            draining := TRUE;               \* ... or takes the role and passes itself
            call Drain();
            return;
        end if;
    end if;
end procedure;

(***************************************************************************)
(* A sender thread runs its program, one operation after the other, then   *)
(* drops its sender handle.                                                *)
(***************************************************************************)
fair process sender \in Senders
variables
    i = 1,              \* the operation it is at
    req = NoReq,        \* that operation's request
    left = 0,           \* values still to put in, or slots still to give back
    sq = 0,             \* how many values this sender has put in so far
    wasGranted = FALSE; \* the cancelled request had been granted first
begin
S_next:
    \* The next operation, or the end of the program.
    if i > Len(Prog[self]) then
        goto S_drop_sender;
    else
        req := <<self, i>>;                 \* this operation's request
    end if;
S_fresh:
    \* The attempt that does not queue (`try_acquire`): one compare-and-swap on the word.
    ahead[req] := {q \in Reqs : status[q] = "waiting"};   \* who was in line when it began
    lateStart[req] := closedBit;                           \* did it begin after the close?
    \* MUTATION try_ignores_close: a closed channel still admits.
    \* MUTATION try_ignores_queue: free slots are taken past the waiters in line.
    if closedBit /\ Mutation # "try_ignores_close" then
        status[req] := "failed";            \* closed: the operation fails at once
        goto S_failed;
    elsif (~queuedBit \/ Mutation = "try_ignores_queue") /\ free >= OpOf(req).n then
        free := free - OpOf(req).n;         \* the slots are free and nobody waits: take them
        held[self] := held[self] + OpOf(req).n;   \* they are this sender's permits now
        status[req] := "done";              \* (record) it has its slots
        if closedBit then admittedAfterClose := TRUE; end if;   \* only a mutation gets here
        goto S_use;
    elsif OpOf(req).op = "try" then
        status[req] := "refused";           \* `try_` never waits: Full
        goto S_op_done;
    else
        status[req] := "trying";            \* it will queue
    end if;
S_push:
    \* `Wait::enqueue`: a node asking for n slots goes on the arrival stack.
    arrivals := Append(arrivals, req);      \* pushed on the stack ...
    nodeState[req] := "waiting";            \* ... as a waiting node ...
    need[req] := OpOf(req).n;               \* ... whose payload is its request
S_enq:
    \* The enqueue's `Step::Drain(state | QUEUED)`: raise QUEUED, and take the drain role
    \* or mark its holder dirty, in one compare-and-swap.
    queuedBit := TRUE;                      \* from now on attempts that do not queue fail
    if status[req] = "trying" then status[req] := "waiting"; end if;   \* now it is in line
    if draining then
        dirty := TRUE;                      \* the role's holder passes again and sees it
        goto S_park;
    else
        draining := TRUE;                   \* this thread serves the queue itself
        call Drain();
        goto S_park;
    end if;
S_park:
    \* `park(None)`: one compare-and-swap arms the waker unless the node is granted already.
    \* MUTATION park_no_look: the waker is armed without reading the node, so a grant that
    \* came first is slept beside.
    if nodeState[req] = "granted" /\ Mutation # "park_no_look" then
        goto S_granted;                     \* the grant came first: no need to sleep
    elsif OpOf(req).op = "cancel" then
        goto S_cancel;                      \* this operation gives up after one poll
    else
        armed[req] := TRUE;                 \* the waker is in place: sleep until it is called
        goto S_wait;
    end if;
S_wait:
    \* The task sleeps until its waker is called.
    await woken[req];                       \* nothing happens to it until then
    woken[req] := FALSE;                    \* the wake is used up
S_repoll:
    \* `park(Some(waker))`: the grant is there, or the waker is armed again.
    if nodeState[req] = "granted" then
        goto S_granted;                     \* granted: look at what it got
    else
        armed[req] := TRUE;                 \* not yet: sleep again
        goto S_wait;
    end if;
S_granted:
    \* The future looks at the grant: the close (an error) or its slots.
    if closedG[req] then
        status[req] := "failed";            \* the close: the operation fails
        goto S_failed;
    else
        held[self] := held[self] + owned[req];   \* the slots become the sender's permits
        owned[req] := 0;                    \* the node carries nothing any more
        status[req] := "done";              \* (record) it has its slots
        goto S_use;
    end if;
S_failed:
    \* The failed wait's drop (`Reserve`'s drop): wake a receiver that waits for a closed
    \* channel to go idle.
    if Finished then wake_rx(); end if;
    goto S_op_done;
S_cancel:
    \* `Wait::cancel`: withdraw the node, unless a grant got there first.
    if nodeState[req] = "granted" then
        wasGranted := TRUE;                 \* too late to withdraw: a grant came first
        if closedG[req] then
            status[req] := "failed";        \* granted the close: nothing to give back
        else
            inHand[self] := inHand[self] + owned[req];   \* granted slots: give them back
            left := owned[req];             \* how many
            owned[req] := 0;                \* the node carries nothing any more
            status[req] := "returned";      \* (record) granted, then given back
        end if;
    else
        wasGranted := FALSE;                \* it withdrew before any grant
        nodeState[req] := "cancelled";      \* withdrawn: no pass grants it from now on
        armed[req] := FALSE;                \* and its waker is dropped with it
        status[req] := "cancelled";         \* (record) it left the line
    end if;
S_cancel_on:
    \* Pass on what the request held: granted slots through a release, a withdrawal
    \* through a draining transition whose pass hands the set-aside to the next waiter.
    if wasGranted /\ left > 0 then
        if Mutation = "drop_pass_on" then
            inHand[self] := inHand[self] - left;   \* MUTATION drop_pass_on: the slots are lost
            goto S_cancel_wake;
        else
            call Release(left);             \* the release hands them to the next waiter
            goto S_cancel_wake;
        end if;
    elsif ~wasGranted then
        if draining then
            dirty := TRUE;                  \* the role's holder passes again and sees it
            goto S_cancel_wake;
        else
            draining := TRUE;               \* this thread serves the queue itself
            call Drain();
            goto S_cancel_wake;
        end if;
    else
        goto S_cancel_wake;                 \* granted the close: nothing to pass on
    end if;
S_cancel_wake:
    \* The cancelled wait's drop wakes a receiver whose closed channel just went idle.
    left := 0;                              \* nothing left to give back
    if Finished then wake_rx(); end if;
    goto S_op_done;
S_use:
    \* With its slots in hand, the operation puts `use` values in (a "cancel" puts none).
    left := IF OpOf(req).op = "cancel" THEN 0 ELSE OpOf(req).use;
S_send:
    \* `Permit::send`: a slot goes to a value, whose push first claims the queue's next cell.
    if left > 0 then
        sq := sq + 1;                       \* this sender's next value ...
        mq := Append(mq, <<self, sq>>);     \* ... claims the cell at the back of the queue
        unready := unready \cup {<<self, sq>>};   \* claimed, not written yet
        pushed := pushed \cup {<<self, sq>>};   \* (record) it was sent
        held[self] := held[self] - 1;       \* the permit is used up: its slot carries the value
        left := left - 1;                   \* one fewer to send
        if rxEnded then pushAfterEnd := TRUE; end if;   \* (record) a value after the end
        \* MUTATION wake_before_write: the receiver is woken before the value is written.
        if Mutation = "wake_before_write" then goto S_early_wake; end if;
    else
        goto S_give_back;
    end if;
S_write:
    \* The push writes the value into its cell: the receiver can take it now.
    unready := unready \ {<<self, sq>>};
S_send_wake:
    \* Then, once the push returned, the receiver's waker.
    wake_rx();
    goto S_send;
S_early_wake:
    \* (mutation wake_before_write only) The wake comes first ...
    wake_rx();
S_late_write:
    \* ... and the value is written after it.
    unready := unready \ {<<self, sq>>};
    goto S_send;
S_give_back:
    \* The permits not used are dropped: their slots go back in one release.
    if held[self] > 0 then
        inHand[self] := inHand[self] + held[self];   \* the permits' slots, on their way back
        left := held[self];                 \* how many
        held[self] := 0;                    \* no permit left
        call Release(left);                 \* back to the word, and on to any waiter
        goto S_give_wake;
    else
        goto S_op_done;
    end if;
S_give_wake:
    \* `Chan::give_back`: wake a receiver whose closed channel just went idle.
    left := 0;                              \* nothing left to give back
    if Finished then wake_rx(); end if;
S_op_done:
    \* On to the next operation.
    i := i + 1;                             \* the next position in the program
    goto S_next;
S_drop_sender:
    \* The sender handle's drop: one fetch_sub on the sender count ...
    txCount := txCount - 1;
S_drop_wake:
    \* ... and the last sender wakes the receiver, whose channel has ended.
    if txCount = 0 then wake_rx(); end if;
end process;

(***************************************************************************)
(* The receiver runs RxProg: "recv" receives once, "drain" receives until  *)
(* the end, "close" closes, "drop" drops the receiver.                     *)
(***************************************************************************)
fair process receiver = Rx
variables ri = 1;   \* the receiver's operation
begin
R_next:
    \* The next operation, or the end of the program.
    if ri > Len(RxProg) then
        goto R_done;
    elsif RxProg[ri] = "close" then
        call Close();
        goto R_op_done;
    elsif RxProg[ri] = "drop" then
        call Close();
        goto R_drop_waker;
    else
        goto R_look;
    end if;
R_look:
    \* `next()`: take the oldest value, or see that every sender is gone.
    if FrontReady then
        take_value();
        goto R_release;
    \* MUTATION skip_busy: the receiver passes over the cell being written and, in a
    \* later step, takes the first written cell behind it (the queue kovan had before).
    elsif FrontBusy /\ Mutation = "skip_busy" then
        goto R_skip;
    \* A cell being written reads as an empty queue does: the sender count is read, and
    \* the send writing it still counts, so this is never the end.
    elsif txCount = 0 then
        goto R_end;                         \* empty, and nobody can send: the end
    else
        goto R_register;
    end if;
R_skip:
    \* (mutation skip_busy only) The first written cell after the oldest, if any.
    if FirstWritten > 0 then
        received := Append(received, mq[FirstWritten]);   \* (record) taken out of order
        mq := RemoveAt(mq, FirstWritten);   \* it leaves the queue
        inHand[Rx] := 1;                    \* its slot, on its way back
        goto R_release;
    else
        goto R_register;
    end if;
R_register:
    \* Nothing yet: register the waker before looking again.
    rxRegistered := TRUE;                   \* the waker cell now holds the receiver's waker
    \* MUTATION rx_no_recheck: the receiver sleeps without looking again.
    if Mutation = "rx_no_recheck" then goto R_sleep; end if;
R_look_again:
    \* The look after the registration: whatever came before it is seen here. A value
    \* still being written is not: its send wakes the receiver after the write.
    if FrontReady then
        take_value();
        goto R_release;
    \* MUTATION finish_early: a closed channel counts as finished before it is idle.
    elsif (mq = <<>> /\ txCount = 0) \/ Finished \/ (Mutation = "finish_early" /\ closedBit) then
        goto R_end;                         \* the end: no sender, or closed and idle
    else
        goto R_sleep;
    end if;
R_sleep:
    \* `Pending`: the receiver sleeps until its waker is called.
    await rxWoken;                          \* nothing happens to it until then
    rxWoken := FALSE;                       \* the wake is used up
    goto R_look;
R_release:
    \* The slot goes back to the senders (`Chan::next` releases one).
    call Release(1);
    goto R_received;
R_received:
    \* "drain" keeps receiving; "recv" is done.
    if RxProg[ri] = "drain" then goto R_look; else goto R_op_done; end if;
R_end:
    \* The receive returned the end of the channel.
    rxEnded := TRUE;
    goto R_op_done;
R_drop_waker:
    \* The receiver's drop lets go of its waker ...
    rxRegistered := FALSE;                  \* the cell is emptied for good
R_drop_values:
    \* ... and drops every value left, giving each one's slot back. It stops at a cell
    \* still being written: that value and those behind it stay in the queue, which is
    \* dropped with the channel.
    if FrontReady then
        dropped := dropped \cup {Head(mq)};   \* (record) the oldest value is thrown away
        mq := Tail(mq);                     \* it leaves the queue
        inHand[Rx] := 1;                    \* its slot, on its way back
        call Release(1);                    \* back to the word, and on to any waiter
        goto R_drop_values;
    else
        goto R_op_done;                     \* the queue is empty: the drop is done
    end if;
R_op_done:
    ri := ri + 1;                           \* the receiver's next operation
    goto R_next;
R_done:
    skip;                                   \* the receiver's program is over
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties. Each says one thing the channel promises.               *)
(***************************************************************************)

\* Status of a request: "new" (not begun), "trying" (its first attempt failed, it is queueing),
\* "waiting" (in line), "granted" (a pass handed it its slots), "done" (its slots are its
\* permits), "returned" (granted, then dropped: slots given back), "cancelled" (dropped while
\* waiting), "failed" (the channel closed), "refused" (a `try_` found no room).

\* Capacity conservation: every slot is in exactly one place, so the counts add up to Cap,
\* and the queue never holds more values than there are slots.
Conservation ==
    /\ free + setAside + SumOver(owned, Reqs) + SumOver(held, Senders)
         + SumOver(inHand, Procs) + Len(mq) = Cap
    /\ Len(mq) <= Cap

\* First come, first served: a request that was already in line when another began is never
\* still waiting once that other one has its slots, a `try_` that took slots included.
FifoFair ==
    \A r \in Reqs : status[r] \in {"granted", "done", "returned"} =>
        \A q \in ahead[r] : status[q] # "waiting"

\* After the close nothing new is admitted: no slot leaves the word, and every operation that
\* began after the close failed (or was refused).
CloseSemantics ==
    /\ ~admittedAfterClose
    /\ \A r \in Reqs : lateStart[r] => status[r] \in {"failed", "refused"}

\* The close's pass hands a set-aside share back to the free count: once the channel is
\* closed and no pass is running, nothing is set aside any more. A share kept there
\* would be lost for good: the free count could never reach the capacity again, so the
\* closed channel would never go idle (the mutation close_keeps_set_aside does this).
CloseReturnsShare == closedBit /\ ~draining => setAside = 0

\* Once a receive returned the end, no value ever goes in again: the receiver had drained
\* every value any slot could still carry.
EndMeansDrained == ~pushAfterEnd

\* A receive returns the end only with nothing in the queue, a cell claimed and not written
\* yet included: the cell being written reads as empty, and its send keeps the sender count
\* above zero, so the count's look never ends the channel in front of it.
EndOnEmpty == pc[Rx] = "R_end" => mq = <<>>
\* Every value is delivered exactly once: received at most once, and every value put in is
\* received, still in the queue, or thrown away by the receiver's drop.
ExactlyOnce ==
    /\ \A j, m \in DOMAIN received : j # m => received[j] # received[m]
    /\ Range(received) \subseteq pushed
    /\ Cardinality(pushed) = Len(received) + Cardinality(dropped) + Len(mq)

\* Each sender's values come out in the order it put them in.
PerSenderOrder ==
    \A j, m \in DOMAIN received :
        j < m /\ received[j][1] = received[m][1] => received[j][2] < received[m][2]

\* All the safety properties together.
Safe == Conservation /\ FifoFair /\ CloseSemantics /\ CloseReturnsShare
        /\ EndMeansDrained /\ ExactlyOnce /\ PerSenderOrder

(***************************************************************************)
(* Witnesses. Each one claims an interesting case never happens; a         *)
(* configuration checks it and expects it to break, which proves the       *)
(* search really reaches that case.                                        *)
(***************************************************************************)

\* Slots stay set aside for a waiting front while no pass runs (a partial reserve_many).
NoShareHeldBetweenPasses == setAside > 0 => draining
\* A pass grants a front that withdrew at that very moment (the share stays for the next).
NoWithdrawAtGrant == \A p \in Procs : pc[p] = "D_grant" => nodeState[front[p]] # "cancelled"
\* A request is granted, then dropped before its future looked: it gives the slots back.
NoGrantThenDrop == \A p \in Senders : pc[p] = "S_cancel" => nodeState[req[p]] # "granted"
\* The front waiter, holding a set-aside share, is dropped.
NoCancelWithShare ==
    \A p \in Senders : (pc[p] = "S_cancel" /\ setAside > 0 /\ list # <<>>) => Head(list) # req[p]
\* An attempt that does not queue finds enough free slots but is refused because a waiter
\* is in line.
NoRefusalForQueue ==
    \A p \in Senders : (pc[p] = "S_fresh" /\ ~closedBit /\ queuedBit) => free < OpOf(req[p]).n
\* A sleeping waiter is woken by the waker a grant claimed.
NoWakeByGrant == \A r \in Reqs : ~woken[r]
\* A drain passes again because the queue changed or a release asked while it worked.
NoPassAgain == \A p \in Procs : pc[p] = "D_exit" => ~(dirty \/ (found[p] /\ wakeFront))
\* The close's pass gives a set-aside share back to the word.
NoCloseRefund == \A p \in Procs : (pc[p] = "D_return" /\ closedBit) => setAside = 0
\* A waiting request is failed by the close.
NoFailByClose == \A r \in Reqs : ~closedG[r]
\* A value goes in after the close, through a slot reserved before it.
NoValueAfterClose == \A p \in Senders : pc[p] = "S_send_wake" => ~closedBit
\* The receiver parks while the oldest cell is claimed and not written yet.
NoParkOnBusy == pc[Rx] = "R_sleep" => ~FrontBusy
\* The receiver ends a closed channel by finding it idle while a sender still lives.
NoEndByIdle == rxEnded => txCount = 0
\* A grant reaches a node between its push and its park, so the future sees it without
\* sleeping.
NoGrantBeforePark == \A p \in Senders : pc[p] = "S_park" => nodeState[req[p]] # "granted"
=============================================================================

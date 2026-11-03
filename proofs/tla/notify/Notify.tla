------------------------------- MODULE Notify -------------------------------
(***************************************************************************)
(* Notify (src/sync/notify.rs: `Notify::notify_with`, `notify_waiters`,   *)
(* `take_permit`, `Policy::pass` for `Notify`, `Core::drive` and           *)
(* `Core::cancel`; on the drain role of src/sync/queue.rs), at the grain   *)
(* of its atomic steps.                                                    *)
(*                                                                          *)
(* In plain words: `notify_one` with nobody queued stores one permit (at   *)
(* most one); with somebody queued it counts one owed wake and asks for a  *)
(* pass, which grants the oldest waiter (`notify_last`: the newest).       *)
(* `notify_waiters` moves a generation counter on and asks for a pass,     *)
(* which grants every waiter of an older generation. A `Notified` takes    *)
(* the generation as it is made; polled, it is done if the generation      *)
(* moved since or it can take the permit with nobody queued ahead, and     *)
(* otherwise queues with its generation. The look at the generation and    *)
(* the take of the permit are two steps (`Core::drive`): a broadcast can   *)
(* land between them, and a permit taken after it is passed on with a      *)
(* `notify_one`, since the broadcast already owed this waiter its wake.    *)
(*                                                                          *)
(* An owed wake is counted in a word that also holds the generation it    *)
(* was made at (`Owed`, one word for `notify_one`, one for `notify_last`): *)
(* the notifier reads the generation, then adds its count in one step.    *)
(* A count of the word's own generation is current; one made at a later    *)
(* generation turns the word's counts old and starts its own; one whose   *)
(* read came before a newer call's is old. A pass reads the generation,   *)
(* then takes both words: the counts of that generation are served to its *)
(* waiters; the older ones belong to the waiters the broadcasts since     *)
(* woke, which the pass expires, each of them taking one (a `notify_one`  *)
(* that came before the broadcast woke it, as the broadcast did); a word  *)
(* of a newer generation (a broadcast and a call came after the read) goes *)
(* back whole, for the pass that broadcast asks for. What is left of the  *)
(* current and older counts becomes the permit. So a broadcast and then a *)
(* `notify_one` while a pass runs leave the permit for a waiter made      *)
(* after both, and a `notify_one` and then a broadcast leave none.        *)
(*                                                                          *)
(* A pass reads the generation after it took the arrivals, grants the      *)
(* queued waiters and the arrivals of an                                   *)
(* older generation, turns a stored permit into a wake for the front, and  *)
(* serves the owed wakes from the front (and the back); what it cannot     *)
(* serve becomes a permit again. A waiter dropped after a grant from       *)
(* `notify_one` (or `notify_last`) passes it on with a `notify_one` (or a *)
(* `notify_last`).                                                         *)
(*                                                                          *)
(* The pass in the code's order (`Policy for Notify`, `pass`): it expires  *)
(* the queued waiters from the front while the front's generation is not  *)
(* the one it read (every node a pass leaves queued carries that pass's    *)
(* generation, so the front reaches them all), then checks each arrival on *)
(* its own (an older one is granted, a current one queued), lets withdrawn *)
(* nodes go only from the ends or in a sweep (`List::tidy`, which runs     *)
(* only past half: here it may run or not), and serves the owed wakes one *)
(* front (or back) node at a time: a node that withdrew between the look  *)
(* and the grant (`WakeList::grant` answering false) is not counted.       *)
(* QUEUED is cleared on the way out only when the list is empty, withdrawn *)
(* nodes included. Two notifiers can call at once (Watch's `send` runs    *)
(* `notify_waiters` from every sender).                                   *)
(*                                                                          *)
(* What is modelled step by step: the state word's PERMIT, QUEUED and the  *)
(* drain role's DRAINING and DIRTY, the generation, the owed counts, the   *)
(* arrival stack and the waiting list, each node's phase and payload, each *)
(* transition, each pass and each grant. What is abstract: the node's     *)
(* waker handshake (a grant reaches a waiting future: the waiter           *)
(* machinery's part) and the list's sweep of withdrawn nodes (one step).   *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    Waiters,   \* per waiter: "await" (polled until done), "drop" (polled once, then dropped),
               \* "early" (as "await", made before the notifier's first call) or "after" (as
               \* "await", made once the notifier's last call returned)
    Notifier,  \* the notifier's calls in order: "one", "last" or "waiters"
    Notifier2, \* a second notifier's calls, at the same time (<<>>: no second notifier)
    NotifyAfterQueued, \* the notifier starts once every waiter has queued (or is done)
    Mutation   \* "none", or the name of the one rule this run takes away

W == 1..Len(Waiters)
NT == 100
NoNode == 0
BY_FIRST == -1
BY_LAST == -2
Range(s) == {s[i] : i \in DOMAIN s}
Without(s, S) == SelectSeq(s, LAMBDA x : x \notin S)
\* The calls of notifier `n`.
Calls(n) == IF n = NT THEN Notifier ELSE Notifier2
\* An owed word with no count: of generation 0.
NoOwed == [tag |-> 0, cur |-> 0, old |-> 0]
Min(a, b) == IF a < b THEN a ELSE b

(* --algorithm Notify
variables
    permit = FALSE,
    queuedBit = FALSE,
    draining = FALSE,
    dirty = FALSE,
    generation = 0,
    owedFirst = NoOwed,              \* `notify_one`'s word: [tag, cur, old]
    owedLast = NoOwed,               \* `notify_last`'s word, the same
    arrivals = <<>>,
    list = <<>>,
    phase = [w \in W |-> "none"],    \* "none", "waiting", "granted", "cancelled"
    payload = [w \in W |-> 0],       \* the node's generation, or BY_FIRST / BY_LAST once granted so
    wGen = [w \in W |-> 0],          \* the generation the Notified took as it was made
    made = [w \in W |-> FALSE],      \* the Notified exists
    done = [w \in W |-> FALSE],
    enqueued = [w \in W |-> FALSE],
    \* records
    ones = 0,          \* notify_one / notify_last calls made (passed-on ones included)
    servedByOne = 0,   \* waiters done through a notify_one / notify_last or a permit
    spurious = FALSE,  \* a waiter done with no notification since it was made
    passedOn = FALSE,  \* a dropped waiter passed its grant on
    eaten = FALSE;     \* a waiter the broadcast owed its wake kept a permit for itself

define
    \* A waiter is done only for a notification made since its Notified was: a generation
    \* that moved, a permit, or a grant.
    NoSpuriousWake == ~spurious
    \* No notify_one serves more than one waiter.
    NoOverServe == servedByOne <= ones
    \* With the drain role free, no waiter whose enqueue happened is left on the arrival stack.
    NoStrandedArrival == \A w \in Range(arrivals) : enqueued[w] => draining
    \* With the drain role free, every waiter still queued carries one generation: the one the
    \* last pass read. That is what lets a pass expire them all from the front.
    QueuedOneGeneration ==
        ~draining => \A a, b \in {x \in Range(list) : phase[x] = "waiting"} : payload[a] = payload[b]
    \* The list with its withdrawn nodes at the front let go (`List::front`).
    RECURSIVE PopCancelled(_)
    PopCancelled(l) == IF l # <<>> /\ phase[Head(l)] = "cancelled" THEN PopCancelled(Tail(l)) ELSE l
    \* The list with its expired front let go: withdrawn nodes, and waiting ones of a generation
    \* other than `gen`, up to the first waiting node of `gen`.
    \* The same at the back (`List::back`).
    RECURSIVE PopCancelledBack(_)
    PopCancelledBack(l) ==
        IF l # <<>> /\ phase[l[Len(l)]] = "cancelled" THEN PopCancelledBack(SubSeq(l, 1, Len(l) - 1)) ELSE l
    RECURSIVE Unexpired(_, _)
    Unexpired(l, gen) ==
        IF l # <<>> /\ (phase[Head(l)] = "cancelled" \/ payload[Head(l)] # gen)
        THEN Unexpired(Tail(l), gen) ELSE l
    \* Every waiter made before the last notify_waiters, and still awaiting, ends done.
    AwaitersMadeBeforeAWaitersCallEnd ==
        <>[](\A w \in W : Waiters[w] = "await" /\ made[w] /\ wGen[w] # generation => done[w])
    \* With as many notify_one calls as awaiting waiters, made after them all, every one ends.
    EveryAwaiterEnds == <>[](\A w \in W : Waiters[w] = "await" => done[w])
    \* Once every thread is done, a notify_one that no waiter took is kept as the permit.
    OneIsKept ==
        (\A p \in W \cup {NT, NT + 1} : pc[p] = "Done") => (permit \/ servedByOne > 0 \/ ones = 0)
    \* A permit goes to a waiter it can wake: one whose generation moved, so a broadcast
    \* already owes it its wake, never keeps a permit for itself.
    NoPermitEatenByOwed == ~eaten
    \* Every waiter made after the notifier's last call ends: the permit a notify_one left
    \* stored is still there for it.
    LateAwaiterEnds == <>[](\A w \in W : Waiters[w] = "after" => done[w])
    \* No waiter made after the notifier's last call is done: a notify_one that came while
    \* every early waiter was queued leaves no permit, whatever broadcast follows it.
    NoSparePermit == \A w \in W : Waiters[w] = "after" => ~done[w]
    \* A count of an owed wake made at generation `gn`, added to word `w` (`Owed::owe`, one
    \* compare-and-swap): one of the word's own generation is current; one of a later generation
    \* turns the word's counts old and starts that generation's; one whose read came before a
    \* newer call's is old.
    \* MUTATION split_word: the tag is ignored and every count is current, as when the counts
    \* were kept apart from the generation.
    Owe(w, gn) == IF Mutation = "split_word" \/ w.tag = gn THEN [w EXCEPT !.cur = @ + 1]
                  ELSE IF w.tag < gn THEN [tag |-> gn, cur |-> 1, old |-> w.old + w.cur]
                  ELSE [w EXCEPT !.old = @ + 1]
    \* What a pass that read generation `g` after it took word `w` serves to the waiters of `g`,
    \* and what it holds as older.
    \* Whether word `w` is of a newer generation than `g`, which the pass gives back.
    \* MUTATION newer_as_old: a newer word is taken as older, its counts given to the waiters the
    \* pass expires, which a broadcast before it woke.
    Newer(w, g) == Mutation # "split_word" /\ Mutation # "newer_as_old" /\ w.tag > g
    CurOf(w, g) == IF Mutation = "split_word" \/ w.tag = g THEN w.cur ELSE 0
    OldOf(w, g) == IF Mutation = "split_word" \/ w.tag = g THEN w.old
                   ELSE IF Newer(w, g) THEN 0
                   ELSE w.old + w.cur
    \* Word `t`, taken, given back to word `w` (one compare-and-swap): of `w`'s generation, its
    \* counts add to `w`'s; `w` of a newer one (another count came since), all of `t`'s are older.
    GiveBack(w, t) == IF w.tag = t.tag THEN [w EXCEPT !.cur = @ + t.cur, !.old = @ + t.old]
                      ELSE [w EXCEPT !.old = @ + t.cur + t.old]
    \* The waiting nodes a pass of generation `g` expires, in line order: the list's front up to
    \* its first node of `g`, then the arrivals of another generation.
    ExpiredSeq(l, a, g) ==
        SelectSeq(SubSeq(l, 1, Len(l) - Len(Unexpired(l, g))), LAMBDA x : phase[x] = "waiting")
        \o SelectSeq(a, LAMBDA x : phase[x] = "waiting" /\ payload[x] # g
                                  /\ Mutation # "arrivals_unchecked")
    \* Witnesses.
    NoPermitStored == ~permit
    NoPassOn == ~passedOn
end define;

macro claimOrDirty(claimed) begin
    if draining then
        dirty := TRUE;
        claimed := FALSE;
    else
        draining := TRUE;
        claimed := TRUE;
    end if;
end macro;

procedure Drain()
variables g = 0, first = 0, last = 0, oldF = 0, oldL = 0, fw = NoOwed, lw = NoOwed,
          wl = {}, n = NoNode, arr = <<>>;
begin
D_clear:
    dirty := FALSE;
D_take:
    \* The arrivals, the generation, then the owed words: each arrival carries the generation or
    \* an older one, and a count of an older one was made before the broadcast that moved it on,
    \* or by a call that overlapped it.
    if Mutation = "generation_first" then
        g := generation;
    end if;
D_absorb:
    \* The arrival stack is taken whole (`WaitQueue::take`).
    arr := arrivals;
    arrivals := <<>>;
D_gen:
    \* MUTATION owed_before_generation: the owed words are taken before the generation is read:
    \* a notify_one made between the take and the read, before a broadcast, is left for a later
    \* pass, which finds the waiters it was owed to expired by this one, and stores it as a permit.
    if Mutation \notin {"generation_first", "owed_before_generation"} then
        g := generation;
    end if;
D_owed_first:
    \* `notify_one`'s word, its counts taken and its generation left (`Owed::take`).
    fw := owedFirst;
    owedFirst := [owedFirst EXCEPT !.cur = 0, !.old = 0];
D_owed_last:
    \* `notify_last`'s, the same.
    lw := owedLast;
    owedLast := [owedLast EXCEPT !.cur = 0, !.old = 0];
D_gen_late:
    \* (The mutation's read, after the take.)
    if Mutation = "owed_before_generation" then
        g := generation;
    end if;
D_counts:
    \* A word of a newer generation than the pass read goes back whole (`Owed::give_back`): its
    \* counts are for the waiters of that generation, which a later pass, asked for by the
    \* broadcast that made it, serves.
    first := CurOf(fw, g);
    last := CurOf(lw, g);
    oldF := OldOf(fw, g);
    oldL := OldOf(lw, g);
    if Newer(fw, g) then
        owedFirst := GiveBack(owedFirst, fw);
    end if;
    if Newer(lw, g) then
        owedLast := GiveBack(owedLast, lw);
    end if;
D_expire:
    \* From the front, while the front is withdrawn or of another generation: a withdrawn
    \* node is let go, a waiting one granted. Then each arrival on its own: a withdrawn one is
    \* let go, one of another generation granted, one of this generation queued. The expired
    \* take the older counts as they are granted, the oldest `notify_one`'s, the next
    \* `notify_last`'s, marked as those calls' grants.
    \* MUTATION arrivals_unchecked: every arrival still waiting is queued, whatever its
    \* generation, so an older one can sit behind a newer front that stops the expiry.
    \* MUTATION old_to_permit: the expired take none of the older counts, which all become the
    \* permit.
    with es = ExpiredSeq(list, arr, g),
         af = IF Mutation = "old_to_permit" THEN 0 ELSE Min(oldF, Len(ExpiredSeq(list, arr, g))),
         al = IF Mutation = "old_to_permit" THEN 0
              ELSE Min(oldL, Len(ExpiredSeq(list, arr, g)) - Min(oldF, Len(ExpiredSeq(list, arr, g)))) do
        wl := Range(es);
        phase := [x \in W |-> IF x \in Range(es) THEN "granted" ELSE phase[x]];
        payload := [x \in W |-> IF \E k \in 1..af : es[k] = x THEN BY_FIRST
                               ELSE IF \E k \in (af + 1)..(af + al) : es[k] = x THEN BY_LAST
                               ELSE payload[x]];
        oldF := oldF - af;
        oldL := oldL - al;
    end with;
    list := Unexpired(list, g) \o SelectSeq(arr, LAMBDA x : phase[x] = "waiting" /\ x \notin wl);
D_tidy:
    \* `List::tidy` sweeps every withdrawn node only once enough of them piled up: here it
    \* may sweep or not.
    either
        list := SelectSeq(list, LAMBDA x : phase[x] # "cancelled");
    or
        skip;
    end either;
D_permit:
    \* A stored permit is a wake for the front: `List::front` lets the withdrawn nodes at the
    \* front go, and with a waiting one there the permit is taken.
    \* MUTATION permit_not_converted: the permit stays where it is.
    if PopCancelled(list) # <<>> /\ permit /\ Mutation # "permit_not_converted" then
        permit := FALSE;
        first := first + 1;
    end if;
    list := PopCancelled(list);
D_first:
    \* While a wake is owed: the front waiting node, the withdrawn ones before it let go
    \* (`List::front`); with none left the loop ends.
    if first > 0 /\ PopCancelled(list) # <<>> then
        n := Head(PopCancelled(list));
        list := Tail(PopCancelled(list));
    else
        if first > 0 then
            list := <<>>;          \* `List::front` let every withdrawn node go and found none
        end if;
        goto D_last;
    end if;
D_first_grant:
    \* `WakeList::grant`: a node that withdrew since the look is not granted, and the owed
    \* wake is not counted as served.
    \* MUTATION count_withdrawn_grant: the wake is counted as served all the same.
    if phase[n] = "waiting" then
        phase[n] := "granted";
        payload[n] := BY_FIRST;
        wl := wl \cup {n};
        first := first - 1;
    elsif Mutation = "count_withdrawn_grant" then
        first := first - 1;
    end if;
    goto D_first;
D_last:
    \* The same from the back (`List::back`).
    if last > 0 /\ PopCancelledBack(list) # <<>> then
        n := PopCancelledBack(list)[Len(PopCancelledBack(list))];
        list := SubSeq(PopCancelledBack(list), 1, Len(PopCancelledBack(list)) - 1);
    else
        if last > 0 then
            list := <<>>;          \* `List::back` let every withdrawn node go and found none
        end if;
        goto D_leftover;
    end if;
D_last_grant:
    \* As at the front: a node that withdrew since the look is not counted.
    if phase[n] = "waiting" then
        phase[n] := "granted";
        payload[n] := BY_LAST;
        wl := wl \cup {n};
        last := last - 1;
    elsif Mutation = "count_withdrawn_grant" then
        last := last - 1;
    end if;
    goto D_last;
D_leftover:
    \* What nobody could take, current or older, is a permit again.
    \* MUTATION leftover_lost: it is dropped.
    if first + last + oldF + oldL > 0 /\ Mutation # "leftover_lost" then
        permit := TRUE;
    end if;
D_exit:
    \* The way out: with the dirty bit up another pass runs; else the role goes, and QUEUED
    \* with it when the pass left the list empty, withdrawn nodes included.
    if dirty /\ Mutation # "exit_ignores_dirty" then
        goto D_clear;
    else
        draining := FALSE;
        if list = <<>> then
            queuedBit := FALSE;
        end if;
    end if;
D_wake:
    return;
end procedure;

\* `notify_with`: a permit with nobody queued, else an owed wake and a pass.
procedure NotifyWith(kind = "one")
variables claimed = FALSE, gn = 0;
begin
N_tr:
    ones := ones + 1;
    if ~queuedBit then
        permit := TRUE;
        return;
    end if;
N_gen:
    \* The generation the count is made at.
    gn := generation;
N_owe:
    \* One compare-and-swap of the word (`Owed::owe`).
    if kind = "one" then
        owedFirst := Owe(owedFirst, gn);
    else
        owedLast := Owe(owedLast, gn);
    end if;
N_drainTr:
    claimOrDirty(claimed);
N_drain:
    if claimed then
        call Drain();
        return;
    else
        return;
    end if;
end procedure;

procedure NotifyWaiters()
variables claimed = FALSE;
begin
NW_bump:
    generation := generation + 1;
NW_tr:
    \* A read-modify-write even with nobody queued; a pass when somebody is.
    if queuedBit /\ Mutation # "waiters_no_pass" then
        claimOrDirty(claimed);
    else
        claimed := FALSE;
    end if;
NW_drain:
    if claimed then
        call Drain();
        return;
    else
        return;
    end if;
end procedure;

fair process notifier \in {NT, NT + 1}
variables i = 1;
begin
T_start:
    \* An "early" waiter is made before the first call.
    await (~NotifyAfterQueued \/ (\A w \in W : enqueued[w] \/ done[w] \/ Waiters[w] = "after"))
          /\ (\A w \in W : Waiters[w] = "early" => made[w]);
T_call:
    while i <= Len(Calls(self)) do
        if Calls(self)[i] = "waiters" then
            call NotifyWaiters();
        else
            call NotifyWith(Calls(self)[i]);
        end if;
T_next:
        i := i + 1;
    end while;
end process;

fair process waiter \in W
variables claimed = FALSE;
begin
W_make:
    \* An "after" waiter is made once the notifier's calls all returned.
    await Waiters[self] = "after" => pc[NT] = "Done";
    wGen[self] := generation;
    made[self] := TRUE;
W_gen:
    \* Done at once if the generation moved since it was made.
    if generation # wGen[self] then
        done[self] := TRUE;
        goto Done;
    end if;
W_take:
    \* Else the permit, if it is free with nobody queued (`take_permit`, one transition): a
    \* broadcast may have moved the generation since the look above.
    if permit /\ (~queuedBit \/ Mutation = "permit_past_queue") then
        permit := FALSE;                      \* the permit is this waiter's now
        done[self] := TRUE;                   \* and the waiter is done
    else
        goto W_enqueue;                       \* no permit: it queues
    end if;
W_recheck:
    \* After the take, the generation once more: moved, the broadcast owed this waiter its
    \* wake, so the permit goes on with a notify_one. MUTATION no_recheck: the waiter keeps it.
    if generation # wGen[self] then
        if Mutation = "no_recheck" then
            servedByOne := servedByOne + 1;   \* (record) the permit served this waiter ...
            eaten := TRUE;                    \* ... which the broadcast had served already
            goto Done;
        else
            call NotifyWith("one");           \* the permit goes to the next waiter, or back
            goto Done;
        end if;
    else
        servedByOne := servedByOne + 1;       \* (record) the permit served this waiter
        goto Done;
    end if;
W_enqueue:
    phase[self] := "waiting";
    payload[self] := wGen[self];
    arrivals := Append(arrivals, self);
W_enqTr:
    queuedBit := TRUE;
    enqueued[self] := TRUE;
    claimOrDirty(claimed);
W_enqDrain:
    if claimed then
        call Drain();
    end if;
W_park:
    \* "await", "early" and "after" wait; "drop" is dropped after its one poll.
    if Waiters[self] = "drop" then
        goto W_drop;
    end if;
W_wait:
    await phase[self] = "granted";
    if payload[self] \in {BY_FIRST, BY_LAST} then
        servedByOne := servedByOne + 1;
    elsif generation = wGen[self] then
        spurious := TRUE;
    end if;
    done[self] := TRUE;
    goto Done;
W_drop:
    \* `Core::cancel`: withdrawn, or granted; a grant of a notify_one is passed on.
    if phase[self] = "waiting" then
        phase[self] := "cancelled";
    elsif phase[self] = "granted" /\ payload[self] \in {BY_FIRST, BY_LAST}
            /\ Mutation # "drop_keeps_grant" then
        passedOn := TRUE;
        call NotifyWith(IF payload[self] = BY_FIRST THEN "one" ELSE "last");
        goto Done;
    else
        goto Done;
    end if;
W_wdTr:
    claimOrDirty(claimed);
W_wdDrain:
    if claimed then
        call Drain();
    end if;
end process;
end algorithm; *)
=============================================================================

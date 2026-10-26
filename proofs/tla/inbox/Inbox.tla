------------------------------ MODULE Inbox ------------------------------
(***************************************************************************)
(* A shard's inbox (src/rt/inbox.rs): a queue linked through the tasks     *)
(* themselves, which other threads push to and the shard alone pops, and   *)
(* the shard's park around it, at the grain of each atomic access.        *)
(*                                                                          *)
(* In plain words: each task has one link, and is in the queue at most     *)
(* once (its scheduled bit). A pusher clears its task's link, swaps the    *)
(* task in as the queue's head, then links the old head to it, and then    *)
(* wakes the shard if it is parked. The shard pops from its own end: past  *)
(* the stub (a node of the queue's own), it hands out a node once it has   *)
(* read the node's link; the last node is handed out only after the stub   *)
(* is pushed behind it, so the shard's end never points at a task it gave  *)
(* away. Between a push's swap and its link the queue is busy: not empty,  *)
(* nothing to take yet. The queue is empty exactly when its head is the    *)
(* stub, which is what the shard looks at once more after it says it      *)
(* parks. A task popped may be pushed again at once, and is, here.        *)
(*                                                                          *)
(* What is abstract: a task is its node; the wake is the park handshake's *)
(* (checked in `park`): a pusher that finds the flag raised lowers it.     *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Producers, \* the pushers, each pushing its own task again and again
    Rounds,    \* how many times each task is pushed
    Mutation   \* "none", or the name of the one rule this run takes away

Stub == "stub"
Null == "null"
Nodes == Producers \cup {Stub}
ShardT == "shard"

(* --algorithm Inbox
variables
    head = Stub,                          \* the pushers' end
    next = [n \in Nodes |-> Null],        \* each node's link
    tail = Stub,                          \* the shard's end (the shard alone touches it)
    queued = [p \in Producers |-> FALSE], \* p's task is in the queue
    pushes = [p \in Producers |-> 0],
    pops = [p \in Producers |-> 0],
    parked = FALSE,
    dup = FALSE,                          \* (record) a task was handed out that was not queued
    busySeen = FALSE,                     \* (record) the shard found the queue busy
    restubbed = FALSE,                    \* (record) the shard pushed the stub behind the last task
    slept = FALSE;                        \* (record) the shard slept and was woken

define
    Done == \A p \in Producers : pops[p] = Rounds
end define;

fair process pusher \in Producers
variables prev = Null;
begin
P_loop:
    while pushes[self] < Rounds do
P_wait:
        \* The task runs, and is woken again only then: one place in the queue.
        await ~queued[self];
P_reset:
        \* MUTATION no_reset: the link keeps what it held from the last time.
        if Mutation # "no_reset" then
            next[self] := Null;
        end if;
P_swap:
        prev := head;
        head := self;
        queued[self] := TRUE;
        pushes[self] := pushes[self] + 1;
P_link:
        next[prev] := self;
P_wake:
        if parked then
            parked := FALSE;
        end if;
    end while;
end process;

fair process shard = ShardT
variables cur = Null, nx = Null, hd = Null, sprev = Null;
begin
S_loop:
    while ~Done do
S_read:
        cur := tail;
        nx := next[cur];
S_stub:
        if cur = Stub then
            if nx = Null then
                if head = Stub then
                    goto S_park;
                else
                    busySeen := TRUE;
                    goto S_loop;
                end if;
            end if;
S_past_stub:
            tail := nx;
            cur := nx;
S_read_past:
            nx := next[cur];
        end if;
S_next:
        if nx # Null then
            tail := nx;
            goto S_take;
        end if;
S_head:
        hd := head;
        if cur # hd then
            busySeen := TRUE;
            goto S_loop;
        end if;
S_last:
        \* MUTATION take_last: the last task is handed out with the shard's
        \* end left on it, no stub pushed behind.
        if Mutation = "take_last" then
            goto S_take;
        end if;
S_restub_reset:
        next[Stub] := Null;
S_restub_swap:
        sprev := head;
        head := Stub;
        restubbed := TRUE;
S_restub_link:
        next[sprev] := Stub;
S_reread:
        nx := next[cur];
        if nx # Null then
            tail := nx;
            goto S_take;
        else
            busySeen := TRUE;
            goto S_loop;
        end if;
S_take:
        if ~queued[cur] then
            dup := TRUE;
        end if;
        queued[cur] := FALSE;
        pops[cur] := pops[cur] + 1;
        goto S_loop;
S_park:
        parked := TRUE;
S_recheck:
        \* MUTATION no_recheck: the shard sleeps without looking again.
        if head # Stub /\ Mutation # "no_recheck" then
            parked := FALSE;
            goto S_loop;
        end if;
S_sleep:
        if ~Done then
            await ~parked;
            slept := TRUE;
        end if;
    end while;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* No task is handed out that is not in the queue: none twice for one push.
NoDuplicate == ~dup

\* Pops never pass pushes.
Counted == \A p \in Producers : pops[p] <= pushes[p]

\* A shard asleep with the head on the stub has no task in its queue: the
\* look it makes after it says it parks is enough.
EmptyAtSleep == (pc[ShardT] = "S_sleep" /\ head = Stub) => \A p \in Producers : ~queued[p]

\* Every push is taken: the shard never sleeps through a task pushed.
AllTaken == <>Done

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* The shard finds a push between its swap and its link.
NoBusy == ~busySeen
\* The shard pushes the stub behind the last task.
NoRestub == ~restubbed
\* The shard sleeps and a push wakes it.
NoSleep == ~slept
=============================================================================

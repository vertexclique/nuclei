------------------------------- MODULE Barrier -------------------------------
(***************************************************************************)
(* The reusable barrier (src/sync/barrier.rs, on the drain role of        *)
(* src/sync/queue.rs), at the grain of its atomic steps.                   *)
(*                                                                          *)
(* In plain words: every arrival ever is counted with one fetch_add.       *)
(* Arrival i belongs to generation i / N, and the one with i % N = N - 1   *)
(* completes it: it is the leader, returns at once, and asks for a pass if *)
(* anybody is queued. Every other arrival queues with its generation. A    *)
(* pass reads the current generation (from the count) after it took the   *)
(* arrivals, grants every queued waiter of an older generation and every   *)
(* arrival of one, and queues the rest. A wait dropped before it completes *)
(* stays counted; only its node goes.                                      *)
(*                                                                          *)
(* What is modelled step by step: the arrival count, the state word's      *)
(* QUEUED and the drain role's bits, the arrival stack and the waiting     *)
(* list, each node's phase and generation, each transition, pass and       *)
(* grant. What is abstract: the node's waker handshake and the list's      *)
(* sweep, as in the other models of the drain role.                        *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    N,         \* the barrier's size
    Rounds,    \* per task, how many waits it makes in turn
    Drops,     \* the tasks whose first wait is dropped after its first poll
    Mutation   \* "none", or the name of the one rule this run takes away

Tasks == 1..Len(Rounds)
Reqs == UNION {{<<t, j>> : j \in 1..Rounds[t]} : t \in Tasks}
NoReq == <<0, 0>>
Range(s) == {s[k] : k \in DOMAIN s}
Without(s, S) == SelectSeq(s, LAMBDA x : x \notin S)
GenOf(a) == a \div N

(* --algorithm Barrier
variables
    count = 0,           \* arrivals ever
    queuedBit = FALSE,
    draining = FALSE,
    dirty = FALSE,
    arrivals = <<>>,
    list = <<>>,
    listGen = 0,         \* the generation the list's waiters are of
    phase = [r \in Reqs |-> "none"],
    gen = [r \in Reqs |-> 0],
    done = [r \in Reqs |-> FALSE],
    leader = [r \in Reqs |-> FALSE],
    enqueued = [r \in Reqs |-> FALSE],
    dropped = [r \in Reqs |-> FALSE],   \* the wait's future was dropped
    \* records
    early = FALSE;       \* a wait completed before its generation did

define
    \* A wait completes only once its generation has every arrival.
    NoEarlyRelease == ~early
    \* One leader per generation.
    OneLeaderEach ==
        \A g \in 0..10 : Cardinality({r \in Reqs : leader[r] /\ gen[r] = g}) <= 1
    NoStrandedArrival == \A r \in Range(arrivals) : enqueued[r] => draining
    \* Every wait whose generation completed, and was not dropped, completes.
    ReleasedTogether ==
        <>[](\A r \in Reqs : ~dropped[r] /\ phase[r] # "none" /\ gen[r] < GenOf(count) => done[r])
    \* Witnesses.
    NoSecondGeneration == \A r \in Reqs : done[r] => gen[r] = 0
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
variables cur = 0, taken = <<>>, release = {}, kept = <<>>;
begin
D_clear:
    dirty := FALSE;
D_take:
    if Mutation = "count_first" then
        cur := GenOf(count);
    end if;
D_absorb:
    \* The arrival stack, then (after it) the current generation.
    taken := arrivals;
    arrivals := <<>>;
    if Mutation # "count_first" then
        cur := GenOf(count);
    end if;
D_pass:
    \* The list's waiters are of one generation: a finished one releases them all. An
    \* arrival of an older generation is granted; one of the current generation queues.
    release := (IF listGen # cur THEN {x \in Range(list) : phase[x] = "waiting"} ELSE {})
               \cup {x \in Range(taken) : phase[x] = "waiting"
                                         /\ (gen[x] # cur \/ Mutation = "grant_current")};
    kept := IF listGen # cur THEN <<>> ELSE list;
    list := SelectSeq(kept \o taken, LAMBDA x : phase[x] = "waiting" /\ x \notin release);
    listGen := cur;
    phase := [x \in Reqs |-> IF x \in release THEN "granted" ELSE phase[x]];
D_exit:
    if dirty /\ Mutation # "exit_ignores_dirty" then
        goto D_clear;
    else
        draining := FALSE;
        if list = <<>> then
            queuedBit := FALSE;
        end if;
    end if;
D_end:
    return;
end procedure;

fair process task \in Tasks
variables j = 1, me = NoReq, a = 0, claimed = FALSE;
begin
T_wait:
    while j <= Rounds[self] do
        me := <<self, j>>;
T_arrive:
        \* One fetch_add: the arrival's generation, and whether it completes it.
        a := count;
        count := count + 1;
        gen[me] := GenOf(a);
        if a % N = N - 1 then
            leader[me] := TRUE;
            done[me] := TRUE;
            goto T_leadTr;
        end if;
T_enqueue:
        phase[me] := "waiting";
        arrivals := Append(arrivals, me);
T_enqTr:
        queuedBit := TRUE;
        enqueued[me] := TRUE;
        claimOrDirty(claimed);
T_enqDrain:
        if claimed then
            call Drain();
        end if;
T_park:
        if self \in Drops /\ j = 1 then
            goto T_drop;
        end if;
T_sleep:
        await phase[me] = "granted";
        early := early \/ count < (gen[me] + 1) * N;
        done[me] := TRUE;
        goto T_next;
T_drop:
        dropped[me] := TRUE;
        if phase[me] = "waiting" then
            phase[me] := "cancelled";
        else
            goto T_next;
        end if;
T_dropTr:
        claimOrDirty(claimed);
T_dropDrain:
        if claimed then
            call Drain();
            goto T_next;
        else
            goto T_next;
        end if;
T_leadTr:
        \* The leader asks for a pass when somebody is queued.
        if queuedBit /\ Mutation # "leader_no_pass" then
            claimOrDirty(claimed);
        else
            claimed := FALSE;
        end if;
T_leadDrain:
        if claimed then
            call Drain();
        end if;
T_next:
        j := j + 1;
    end while;
end process;
end algorithm; *)
=============================================================================

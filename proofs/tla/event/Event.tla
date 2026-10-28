------------------------------- MODULE Event -------------------------------
(***************************************************************************)
(* The one-shot event and the latch built on it (src/sync/event.rs,       *)
(* src/sync/latch.rs, on the drain role of src/sync/queue.rs), at the     *)
(* grain of their atomic steps.                                            *)
(*                                                                          *)
(* In plain words: an event is a SET bit. A wait is done at once if the   *)
(* bit is up, and otherwise queues. `set` raises the bit, with a pass when *)
(* somebody is queued; a pass grants every waiter once the bit is up.      *)
(* Setting twice does nothing. A latch holds a count; `count_down` lowers  *)
(* it by one with a compare-and-swap, and the call that takes it to zero   *)
(* sets the event; at zero it does nothing.                                *)
(*                                                                          *)
(* What is modelled step by step: the SET bit, QUEUED and the drain role's *)
(* bits, the arrival stack and the waiting list, each node's phase, each   *)
(* transition, pass and grant, and the latch's count with each             *)
(* compare-and-swap. What is abstract: the node's waker handshake and the  *)
(* list's sweep, as in the other models of the drain role.                 *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    Waiters,    \* per waiter: "await" or "drop" (dropped after its first poll)
    Counters,   \* how many threads count the latch down (0: the event is set directly)
    Count,      \* the latch's count
    Mutation    \* "none", or the name of the one rule this run takes away

W == 1..Len(Waiters)
SetterT == 100
Cs == 200..(199 + Counters)
Range(s) == {s[k] : k \in DOMAIN s}

(* --algorithm Event
variables
    setBit = FALSE,
    queuedBit = FALSE,
    draining = FALSE,
    dirty = FALSE,
    arrivals = <<>>,
    list = <<>>,
    phase = [w \in W |-> "none"],
    done = [w \in W |-> FALSE],
    enqueued = [w \in W |-> FALSE],
    dropped = [w \in W |-> FALSE],
    count = Count,
    countDowns = 0,     \* (record) count_down calls that lowered the count
    early = FALSE,      \* (record) a wait done before the event was set
    openedEarly = FALSE;\* (record) the latch set the event before its count reached zero

define
    NoEarlyWake == ~early
    LatchOpensAtZero == ~openedEarly
    NoStrandedArrival == \A w \in Range(arrivals) : enqueued[w] => draining
    \* Once the event is set, every waiter that was not dropped ends.
    AllWokenAfterSet == <>[](setBit => \A w \in W : ~dropped[w] => done[w])
    \* The latch opens once every count_down came.
    OpensWhenCounted == <>[](countDowns = Count => setBit)
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
variables isSet = FALSE, taken = <<>>;
begin
D_clear:
    dirty := FALSE;
D_absorb:
    taken := arrivals;
    arrivals := <<>>;
D_pass:
    \* After the arrivals were taken: is the event set? Then every waiter is granted.
    isSet := setBit;
    if isSet then
        phase := [x \in W |-> IF x \in Range(list \o taken) /\ phase[x] = "waiting"
                               THEN "granted" ELSE phase[x]];
        list := <<>>;
    else
        list := SelectSeq(list \o taken, LAMBDA x : phase[x] = "waiting");
    end if;
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

\* `Event::set`: Keep if set, a draining transition if somebody is queued.
procedure Set()
variables claimed = FALSE;
begin
S_tr:
    if setBit then
        claimed := FALSE;
    elsif queuedBit /\ Mutation # "set_no_pass" then
        setBit := TRUE;
        claimOrDirty(claimed);
    else
        setBit := TRUE;
        claimed := FALSE;
    end if;
S_drain:
    if claimed then
        call Drain();
        return;
    else
        return;
    end if;
end procedure;

fair process setter \in (IF Counters = 0 THEN {SetterT} ELSE {})
begin
P_set:
    call Set();
P_again:
    \* Setting twice does nothing.
    call Set();
end process;

fair process counter \in Cs
variables seen = 0;
begin
C_load:
    seen := count;
C_cas:
    if seen = 0 then
        goto Done;
    elsif count = seen then
        count := seen - 1;
        countDowns := countDowns + 1;
        if seen = 1 \/ (Mutation = "opens_early" /\ seen = 2) then
            openedEarly := openedEarly \/ seen # 1;
            goto C_set;
        else
            goto Done;
        end if;
    else
        goto C_load;
    end if;
C_set:
    call Set();
end process;

fair process waiter \in W
variables claimed = FALSE;
begin
W_first:
    if setBit then
        done[self] := TRUE;
        goto Done;
    end if;
W_enqueue:
    phase[self] := "waiting";
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
    if Waiters[self] = "drop" then
        goto W_drop;
    end if;
W_sleep:
    await phase[self] = "granted";
    early := early \/ ~setBit;
    done[self] := TRUE;
    goto Done;
W_drop:
    dropped[self] := TRUE;
    if phase[self] = "waiting" then
        phase[self] := "cancelled";
    else
        goto Done;
    end if;
W_dropTr:
    claimOrDirty(claimed);
W_dropDrain:
    if claimed then
        call Drain();
    end if;
end process;
end algorithm; *)
=============================================================================

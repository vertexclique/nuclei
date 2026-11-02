-------------------------------- MODULE Wheel --------------------------------
(***************************************************************************)
(* A shard's timing wheel as a data structure (src/time/wheel.rs:          *)
(* `Wheel::insert`, `Wheel::remove`, `Wheel::advance`, `next_slot`,        *)
(* `link`, `release`, `level_for`, `slot_for`, `TimerKey`), shrunk to      *)
(* 2^B slots per level and L levels, one call at a time.                   *)
(*                                                                          *)
(* In plain words: a timer goes to the lowest level whose window holds     *)
(* both the wheel's time and its deadline (a deadline past the span is     *)
(* placed at the span's edge), in the slot its deadline falls in. An       *)
(* advance to `now` takes the occupied slot with the earliest time, moves  *)
(* the wheel's time there, fires every timer of it whose deadline passed   *)
(* and places the others again (on a lower level), until the next slot is  *)
(* later than `now`; then the wheel's time is `now`. Timers live in a     *)
(* slab: a fired or removed timer's entry goes back to the free list with  *)
(* its generation moved on, so the key of a timer that is gone never      *)
(* reaches the entry's next timer. A deadline the wheel passed already is *)
(* fired as it is armed: its key reaches no entry.                         *)
(*                                                                          *)
(* Who acts: the owners of timers (each arms, may remove, may arm again)  *)
(* and the shard's clock, which advances the wheel; the wheel is the       *)
(* shard's alone, so each call is one step. What must never happen: a     *)
(* timer fired before its deadline; a timer still armed after an advance  *)
(* past its deadline; a removal through a stale key that takes another     *)
(* owner's timer; a next deadline later than an armed timer's.            *)
(*                                                                          *)
(* What is abstract: a slot's list is a set (an advance takes the whole    *)
(* slot); wakers are the fired mark; the generation is an unbounded       *)
(* integer (its u32 wrap is a stated ceiling, not modelled).               *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    B,         \* bits of a slot index: 2^B slots per level
    L,         \* levels
    Owners,    \* the owners of timers
    Arms,      \* how many times each owner may arm a timer
    MaxAhead,  \* the furthest ahead of the clock a deadline is set
    MaxStep,   \* the most one advance moves the clock
    Horizon,   \* the clock stops here
    Mutation   \* "none", or the name of the one rule this run takes away

Slots == 2^B
Span == 2^(B * L)
Entries == 1..Cardinality(Owners)
Free == [lvl |-> -1, slot |-> -1]
FiredKey == [index |-> 0, gen |-> 0]
Pow(e) == 2^e

\* `slot_for`: the slot of `when` on `level`.
SlotFor(when, level) == (when \div Pow(level * B)) % Slots
\* `level_for`: the lowest level whose window holds both times, else the top one.
LevelFor(elapsed, when) ==
    LET fits == {l \in 0..(L - 1) : elapsed \div Pow(B * (l + 1)) = when \div Pow(B * (l + 1))}
    IN IF fits = {} THEN L - 1 ELSE CHOOSE l \in fits : \A m \in fits : l <= m

(* --algorithm Wheel
variables
    now = 0,                                   \* the shard's clock
    elapsed = 0,                               \* the wheel's time
    dl = [e \in Entries |-> 0],                \* each entry's deadline (`Entry::when`)
    place = [e \in Entries |-> Free],          \* its level and slot, or free
    gen = [e \in Entries |-> 0],               \* its generation
    owner = [e \in Entries |-> "none"],        \* (record) whose timer the entry holds
    key = [o \in Owners |-> FiredKey],         \* each owner's key
    arms = [o \in Owners |-> 0],               \* timers each owner armed
    due = [o \in Owners |-> -1],               \* (record) its timer's deadline, -1: none
    early = FALSE,                             \* (record) a timer fired before its deadline
    stolen = FALSE;                            \* (record) a removal took another's timer

define
    Armed == {e \in Entries : place[e] # Free}
    \* `next_slot`: the earliest time of an occupied slot, at or after the wheel's time.
    SlotTime(lvl, s) ==
        LET shift == lvl * B
            range == Pow(shift + B)
            window == elapsed - (elapsed % range)
            nowSlot == SlotFor(elapsed, lvl)
        IN IF s > nowSlot THEN window + s * Pow(shift) ELSE window + range + s * Pow(shift)
    Occupied == {<<place[e].lvl, place[e].slot>> : e \in Armed}
    NextTime ==
        IF Occupied = {} THEN -1
        ELSE LET ts == {SlotTime(p[1], p[2]) : p \in Occupied}
             IN CHOOSE t \in ts : \A u \in ts : t <= u
    \* The slot `next_slot` picks: of those at the earliest time, the lowest level's.
    NextSlot ==
        CHOOSE p \in Occupied : SlotTime(p[1], p[2]) = NextTime
            /\ \A q \in Occupied : SlotTime(q[1], q[2]) = NextTime => p[1] <= q[1]
    \* `link`'s place for a deadline, from the wheel's time: at the span's edge at most.
    Place(d) ==
        LET placed == IF d < elapsed + Span - 1 THEN d ELSE elapsed + Span - 1
            lvl == LevelFor(elapsed, placed)
        IN [lvl |-> lvl, slot |-> SlotFor(placed, lvl)]
    \* No timer fires before its deadline.
    NeverEarly == ~early
    \* Between calls: no timer armed whose deadline the clock passed.
    NeverLate == pc["shard"] = "Pick" => \A e \in Armed : dl[e] > now
    \* A removal through a key takes only the key's own owner's timer.
    KeyIsolation == ~stolen
    \* The next deadline the shard parks for is no later than any armed timer's.
    NextOnTime == pc["shard"] = "Pick" => \A e \in Armed : NextTime <= dl[e]
end define;

\* The shard runs one call at a time.
fair process shard = "shard"
variables todo = {};    \* the timers of the slot an advance took
begin
Pick:
    either
        \* An owner with no timer armed arms one (`insert`).
        with o \in {x \in Owners : arms[x] < Arms /\ due[x] = -1},
             d \in (IF now > 0 THEN now - 1 ELSE 0)..(now + MaxAhead),
             x \in {y \in Entries : place[y] = Free} do
            arms[o] := arms[o] + 1;
            \* A deadline the wheel passed: fired at once, a key that reaches nothing.
            \* MUTATION insert_passed_links: it is linked like any other.
            if d <= elapsed /\ Mutation # "insert_passed_links" then
                key[o] := FiredKey;
            else
                \* `alloc`, then `link`: placed at the span's edge at most.
                dl[x] := d;
                owner[x] := o;
                due[o] := d;
                key[o] := [index |-> x, gen |-> gen[x]];
                place[x] := Place(d);
            end if;
        end with;
        goto Pick;
    or
        \* An owner removes its timer by its key (`remove`); a stale key does nothing.
        \* MUTATION no_generation: the key's generation is not compared.
        with o \in Owners do
            if key[o].index # 0 /\ place[key[o].index] # Free
               /\ (gen[key[o].index] = key[o].gen \/ Mutation = "no_generation") then
                stolen := stolen \/ owner[key[o].index] # o;
                due[owner[key[o].index]] := -1;
                place[key[o].index] := Free;
                gen[key[o].index] := gen[key[o].index] + 1;
                owner[key[o].index] := "none";
            end if;
            key[o] := FiredKey;
        end with;
        goto Pick;
    or
        \* The clock moves on and the shard advances the wheel to it.
        await now < Horizon;
        with step \in 1..MaxStep do
            now := IF now + step > Horizon THEN Horizon ELSE now + step;
        end with;
        goto Advance;
    or
        await now = Horizon;
        goto Done;
    end either;
Advance:
    \* `advance`: the earliest occupied slot, while it is due by `now`.
    if NextTime # -1 /\ NextTime <= now then
        todo := {x \in Armed : <<place[x].lvl, place[x].slot>> = NextSlot};
        elapsed := NextTime;
    else
        \* MUTATION no_elapsed_max: the wheel's time is left at the last slot it took.
        if Mutation # "no_elapsed_max" then
            elapsed := IF elapsed > now THEN elapsed ELSE now;
        end if;
        goto Pick;
    end if;
Slot:
    \* Each timer of the slot: due, it fires and its entry goes free with its generation
    \* moved on; not due, it is placed again from the new time.
    \* MUTATION cascade_fires_all: every timer of the slot fires.
    if todo # {} then
        with x \in todo do
            todo := todo \ {x};
            if dl[x] <= now \/ Mutation = "cascade_fires_all" then
                early := early \/ dl[x] > now;
                due[owner[x]] := -1;
                place[x] := Free;
                gen[x] := gen[x] + 1;
                owner[x] := "none";
            else
                place[x] := Place(dl[x]);
            end if;
        end with;
        goto Slot;
    else
        goto Advance;
    end if;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

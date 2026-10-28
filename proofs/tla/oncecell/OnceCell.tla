------------------------------ MODULE OnceCell ------------------------------
(***************************************************************************)
(* The once-cell (src/sync/once_cell.rs), at the grain of its atomic steps.*)
(*                                                                          *)
(* In plain words: one atomic pointer says where the value is: nowhere     *)
(* (null), in the cell's own slot (a marker), or in a box a racing builder *)
(* published. The turn to write the slot is a one-permit semaphore. An     *)
(* asynchronous initializer looks for the value, waits for the turn, looks *)
(* again, runs its initializer (which may fail, or be dropped), writes the *)
(* slot and publishes it with one compare-and-swap; won, it keeps the turn *)
(* for good and closes the semaphore, which ends every wait for the turn;  *)
(* lost to a racing builder, it takes its value back out of the slot and   *)
(* answers the builder's. A failed or dropped initializer gives the turn   *)
(* back, and the next waiter takes over. `set` takes the turn only if it   *)
(* is free at once. A racing builder builds a box off to the side and lets *)
(* one compare-and-swap decide, closing the semaphore if it won.           *)
(*                                                                          *)
(* What is modelled step by step: the value pointer and each              *)
(* compare-and-swap on it, the slot's write and take-back, the turn (its   *)
(* permit, the close and each wait for it), each look for the value, and   *)
(* each initializer's run and its outcome. What is abstract: the turn's    *)
(* semaphore is one permit, a closed bit and waits that end when the       *)
(* permit is free or the semaphore closed; its own protocol (barging,      *)
(* nudges, the bounded bypass, the close's grants) is the semaphore        *)
(* family's.                                                               *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    Inits,     \* per async initializer: "ok", "fail" (its initializer fails) or "drop" (dropped mid-run)
    Racers,    \* how many racing builders
    Setters,   \* how many `set` calls
    Mutation   \* "none", or the name of the one rule this run takes away

A == 1..Len(Inits)
RacerT == 100..(99 + Racers)
SetterT == 200..(199 + Setters)
None == <<>>
Null == [kind |-> "null", v |-> None]
Inline == [kind |-> "inline", v |-> None]
IsNull(p) == p.kind = "null"

(* --algorithm OnceCell
variables
    ptr = Null,        \* Null, Inline, or a box [kind |-> "box", v |-> its value]
    slot = None,       \* the slot's contents: None, or the value an initializer wrote
    slotWriters = 0,   \* threads writing or holding a write in the slot (record)
    permit = TRUE,     \* the turn is free
    closed = FALSE,
    \* records
    answers = {},      \* every value any call answered
    published = None,  \* the value the pointer published, once it did
    initAfterPublish = FALSE,   \* an initializer ran though the value was there as it looked
    lostToRacer = FALSE,
    twoWriters = FALSE;

define
    Value(p) == IF p.kind = "inline" THEN slot ELSE p.v
    \* Once published, the value is the one every call answers.
    OneValue == \A v \in answers : v = published
    \* The slot has one writer at a time.
    OneWriter == ~twoWriters
    \* Nobody runs an initializer over a value that was there as it took the turn.
    NoInitAfterPublish == ~initAfterPublish
    AllDone == \A p \in DOMAIN pc : pc[p] = "Done"
    \* Every call ends: a dropped or failed initializer gives the turn on.
    EveryCallEnds == <>AllDone
    \* With an initializer that succeeds, the cell ends holding a value.
    NoLostPublish == ~lostToRacer
    FilledInTheEnd == <>[](AllDone => ~IsNull(ptr) \/ \A i \in A : Inits[i] # "ok")
end define;

fair process init \in A
variables mine = None;
begin
I_look:
    if ~IsNull(ptr) then
        answers := answers \cup {Value(ptr)};
        goto Done;
    end if;
I_turn:
    \* `acquire(1)`: ends with the permit, or with the close.
    await permit \/ closed;
    if closed then
        goto I_closed;
    else
        permit := FALSE;
    end if;
I_relook:
    \* A racing builder may have published while this waited.
    if ~IsNull(ptr) then
        if Mutation # "skip_relook" then
            answers := answers \cup {Value(ptr)};
            goto I_giveBack;
        else
            initAfterPublish := TRUE;
        end if;
    end if;
I_run:
    if Inits[self] = "fail" \/ Inits[self] = "drop" then
        \* Failed or dropped: the turn goes back (its permit is dropped).
        goto I_giveBack;
    end if;
I_write:
    twoWriters := twoWriters \/ slotWriters > 0;
    slotWriters := slotWriters + 1;
    mine := <<"init", self>>;
    slot := mine;
I_publish:
    if IsNull(ptr) then
        ptr := Inline;
        published := mine;
        answers := answers \cup {mine};
        goto I_close;
    else
        \* A racing builder won: the value comes back out of the slot.
        lostToRacer := TRUE;
        answers := answers \cup {Value(ptr)};
        slot := None;
        slotWriters := slotWriters - 1;
        goto I_giveBack;
    end if;
I_close:
    \* The turn is kept for good; the close ends every wait for it.
    if Mutation # "no_close" then
        closed := TRUE;
    end if;
    goto Done;
I_giveBack:
    if Mutation # "fail_keeps_turn" \/ Inits[self] # "fail" then
        permit := TRUE;
    end if;
    goto Done;
I_closed:
    \* A closed turn queue means a value is there.
    answers := answers \cup {Value(ptr)};
end process;

fair process racer \in RacerT
variables mine = Null;
begin
R_look:
    if ~IsNull(ptr) then
        answers := answers \cup {Value(ptr)};
        goto Done;
    end if;
R_build:
    mine := [kind |-> "box", v |-> <<"racer", self>>];
R_publish:
    if IsNull(ptr) then
        ptr := mine;
        published := mine.v;
        answers := answers \cup {mine.v};
    else
        answers := answers \cup {Value(ptr)};
        goto Done;
    end if;
R_close:
    if Mutation # "no_close" then
        closed := TRUE;
    end if;
end process;

fair process setter \in SetterT
variables mine = None;
begin
S_look:
    if ~IsNull(ptr) then
        goto Done;
    end if;
S_turn:
    \* `try_acquire_permit(1)`: the turn only if it is free now.
    if permit /\ ~closed then
        permit := FALSE;
    elsif Mutation = "set_without_turn" then
        skip;
    else
        goto Done;
    end if;
S_write:
    twoWriters := twoWriters \/ slotWriters > 0;
    slotWriters := slotWriters + 1;
    mine := <<"set", self>>;
    slot := mine;
S_publish:
    if IsNull(ptr) then
        ptr := Inline;
        published := mine;
        goto S_close;
    else
        slot := None;
        slotWriters := slotWriters - 1;
        permit := TRUE;
        goto Done;
    end if;
S_close:
    closed := TRUE;
end process;
end algorithm; *)
=============================================================================

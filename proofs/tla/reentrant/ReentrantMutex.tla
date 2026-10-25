--------------------------- MODULE ReentrantMutex ---------------------------
(***************************************************************************)
(* The reentrant mutex (src/sync/reentrant_mutex.rs, src/sync/owner.rs),  *)
(* at the grain of its atomic steps.                                       *)
(*                                                                          *)
(* In plain words: the lock is a one-permit semaphore, a holder id (an     *)
(* owner's, or none) and a depth only the holder touches. An owner that    *)
(* finds its own id in the holder locks again at once, one level deeper.   *)
(* Any other takes the permit (waiting for it if it must), sets the depth  *)
(* to one and then stores its id. An unlock takes one level off; the last  *)
(* clears the holder and then gives the permit back. Owner ids are unique, *)
(* so an owner finds its own id only while it holds the lock.              *)
(*                                                                          *)
(* What is modelled step by step: the holder's loads and stores, the       *)
(* depth, and the permit's take, wait and release. What is abstract: the  *)
(* semaphore is one permit with waits that end when it is free; its own   *)
(* protocol is the semaphore family's.                                     *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    Progs,     \* per owner, its steps in order: "L" locks (again, or for the first time), "U" unlocks
    Mutation   \* "none", or the name of the one rule this run takes away

Owners == 1..Len(Progs)
NoOwner == 0

(* --algorithm ReentrantMutex
variables
    permit = TRUE,
    holder = NoOwner,
    depth = 0,
    guards = [o \in Owners |-> 0],   \* the guards each owner holds
    falseReenter = FALSE;            \* (record) an owner holding no guard found its own id

define
    Holders == {o \in Owners : guards[o] > 0}
    \* One owner holds the lock at a time.
    Exclusion == Cardinality(Holders) <= 1
    \* While an owner holds the lock with its id stored, the depth is its guards.
    DepthMatches == \A o \in Holders : holder = o => depth = guards[o]
    \* Only the holder ever finds its own id: a lock again is always the holder's.
    NoFalseReenter == ~falseReenter
    AllDone == \A o \in Owners : pc[o] = "Done"
    FreeAtEnd == AllDone => permit /\ holder = NoOwner /\ depth = 0
    AllOwnersEnd == <>AllDone
    \* Witnesses.
    OneLevel == depth <= 1
    NoWaitBehind == ~\E o \in Owners : pc[o] = "O_take" /\ ~permit
end define;

fair process owner \in Owners
variables i = 1;
begin
O_step:
    while i <= Len(Progs[self]) do
        if Progs[self][i] = "L" then
O_look:
            \* `reenter`: a relaxed load of the holder.
            if holder = self then
                falseReenter := falseReenter \/ guards[self] = 0;
                depth := depth + (IF Mutation = "reenter_skips_depth" THEN 0 ELSE 1);
                guards[self] := guards[self] + 1;
            else
O_take:
                await permit;
                permit := FALSE;
O_enter:
                depth := 1;
                guards[self] := guards[self] + 1;
O_store:
                holder := self;
            end if;
        else
O_down:
            depth := depth - 1;
            guards[self] := guards[self] - 1;
            if depth = 0 then
                if Mutation = "release_before_clear" then
                    goto O_releaseFirst;
                end if;
O_clear:
                if Mutation # "never_clear" then
                    holder := NoOwner;
                end if;
O_release:
                permit := TRUE;
                goto O_next;
O_releaseFirst:
                permit := TRUE;
O_clearLate:
                holder := NoOwner;
            elsif Mutation = "clear_each_unlock" then
                holder := NoOwner;
            end if;
        end if;
O_next:
        i := i + 1;
    end while;
end process;
end algorithm; *)
=============================================================================

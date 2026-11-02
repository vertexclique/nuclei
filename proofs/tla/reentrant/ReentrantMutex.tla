--------------------------- MODULE ReentrantMutex ---------------------------
(***************************************************************************)
(* The reentrant mutex (src/sync/reentrant_mutex.rs: `reenter`, `enter`,  *)
(* `unlock` and `ReentrantMutexLock::poll`; src/sync/owner.rs), at the     *)
(* grain of its atomic steps.                                              *)
(*                                                                          *)
(* In plain words: the lock is a one-permit semaphore, a holder id (an     *)
(* owner's, or none) and a depth only the holder touches. An owner that    *)
(* finds its own id in the holder locks again at once, one level deeper.   *)
(* Any other takes the permit (waiting for it if it must), sets the depth  *)
(* to one and then stores its id. An unlock takes one level off; the last  *)
(* clears the holder and then gives the permit back. Owner ids are unique, *)
(* so an owner finds its own id only while it holds the lock.              *)
(*                                                                          *)
(* One owner may have two lock requests out at once (`join!` of two         *)
(* `lock(&me)`): both queue while another owner holds the lock, the older  *)
(* is handed the permit, and the owner keeps that guard until the younger  *)
(* completes too. Every poll of a request looks for its own id first,       *)
(* queued or not, so the younger, polled after the older entered, leaves   *)
(* the queue and enters at once. A younger request polled before the older *)
(* entered (the join polls it first, or it is in another task of the same  *)
(* owner) is polled again only when it is woken: the owner's entry wakes   *)
(* every other request of that owner still queued on the lock, which then  *)
(* finds the id and enters (without that wake the owner waits on itself:   *)
(* the `no_sibling_wake` mutation).                                        *)
(*                                                                          *)
(* A thread's own token (`Owner::thread`, `SharedIds`) is one id for every *)
(* task of that thread: one task's lock is another's lock again, so two    *)
(* tasks of the thread hold the lock at once, interleaved at their awaits. *)
(* Sound (they run on one thread, so no `&T` crosses threads), but no     *)
(* exclusion between them: the witness of a documented limit, not a bug.   *)
(*                                                                          *)
(* What is modelled step by step: the holder's loads and stores, the       *)
(* depth, and the permit's take, wait and release. What is abstract: the  *)
(* semaphore is one permit with waits that end when it is free; its own   *)
(* protocol is the semaphore family's.                                     *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    Progs,     \* per owner, its steps in order: "L" locks (again, or for the first time), "U" unlocks,
               \* "J" joins two locks polled older first, "JR" the same polled younger first
               \* (each leaves the owner holding two guards)
    SharedIds, \* TRUE: the programs are tasks of one thread, each locking with that thread's one
               \* token (`Owner::thread`), so every one of them has the same id
    Mutation   \* "none", or the name of the one rule this run takes away

Owners == 1..Len(Progs)
NoOwner == 0
\* The id program `t` locks with: its own token's, or with SharedIds its thread's, the same for all.
Id(t) == IF SharedIds THEN 1 ELSE t

(* --algorithm ReentrantMutex
variables
    permit = TRUE,
    holder = NoOwner,
    depth = 0,
    guards = [o \in Owners |-> 0],   \* the guards each owner holds
    falseReenter = FALSE,            \* (record) an owner holding no guard found its own id
    siblingWoken = [o \in Owners |-> FALSE]; \* the owner's queued younger request was woken

define
    Holders == {o \in Owners : guards[o] > 0}
    \* One owner holds the lock at a time (one program: with SharedIds, one task of the thread).
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
        if Progs[self][i] \in {"J", "JR"} then
J_look:
            \* The join's first pass polls the older request: its own id (not here: a join begins
            \* with nothing held), or the free permit, taken at once.
            if permit then
                permit := FALSE;                    \* the older takes the free permit ...
                depth := 1;                         \* ... sets the depth to one ...
                guards[self] := guards[self] + 1;   \* ... and has its guard
                goto J_storeNow;
            else
                goto J_wait;                        \* another owner holds it: both requests queue
            end if;
J_storeNow:
            \* The older stores the owner's id ...
            holder := Id(self);                         \* the older's store of the id
J_youngNow:
            \* ... and the same pass then polls the younger, which finds that id and enters at
            \* once, one level deeper.
            depth := depth + 1;                     \* the younger's level
            guards[self] := guards[self] + 1;       \* and its guard
            goto O_next;
J_wait:
            \* The task sleeps until the permit is handed to the older request, queued first.
            await permit;                           \* the older's grant
            permit := FALSE;                        \* the permit is the owner's now
            if Progs[self][i] = "JR" then
                goto J_youngFirst;                  \* the waking pass polls the younger first
            else
                goto J_enter;                       \* the waking pass polls the older first
            end if;
J_youngFirst:
            \* (JR) The younger is polled while the older has not stored the id: another holder,
            \* as far as it can tell, so it stays queued.
            skip;                                   \* its look finds no id of its own
J_enter:
            \* The older's `enter`: the depth is one ...
            depth := 1;                             \* one level
            guards[self] := guards[self] + 1;       \* and the older's guard
J_store:
            \* ... then the id; the entry wakes the owner's other requests still queued on the
            \* lock. MUTATION no_sibling_wake: nobody is woken.
            holder := Id(self);                         \* the owner holds the lock now
            if Progs[self][i] = "JR" then
                siblingWoken[self] := Mutation # "no_sibling_wake";
                goto J_youngWait;                   \* the younger was polled already
            end if;
J_young:
            \* The same pass polls the younger: queued, it still looks for its own id first and,
            \* finding it, leaves the queue and enters. MUTATION queued_skips_reenter: a queued
            \* request only looks at its node, and sleeps on.
            if holder = Id(self) /\ Mutation # "queued_skips_reenter" then
                depth := depth + 1;                 \* one level deeper
                guards[self] := guards[self] + 1;   \* the younger's guard
                goto O_next;
            end if;
J_youngWait:
            \* The younger sleeps until it is woken: by the owner's entry, which has it poll again,
            \* find its own id and enter one level deeper; else only by the permit handed to it,
            \* which only the owner's last unlock does, and the join keeps the older's guard
            \* until the younger ends.
            await siblingWoken[self] \/ permit;
            if siblingWoken[self] /\ holder = Id(self) then
                siblingWoken[self] := FALSE;        \* the wake seen
                depth := depth + 1;                 \* the younger's level
                guards[self] := guards[self] + 1;   \* and its guard
            else
                permit := FALSE;                    \* (unreachable) the younger's grant
                depth := 1;                         \* (unreachable) its depth
                guards[self] := guards[self] + 1;   \* (unreachable) its guard
            end if;
        elsif Progs[self][i] = "L" then
O_look:
            \* `reenter`: a relaxed load of the holder.
            if holder = Id(self) then
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
                holder := Id(self);
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

---------------------------- MODULE RemoteFree ----------------------------
(***************************************************************************)
(* Task blocks freed on a thread other than their owner's going back to    *)
(* the owner (src/rt/task/memory.rs), at the grain of each atomic access.  *)
(*                                                                          *)
(* In plain words: each thread that allocates task blocks holds a record:  *)
(* a list head other threads push the blocks they free onto, linked        *)
(* through the blocks themselves, and a count that bounds the list. A     *)
(* freeing thread counts itself in, gives up if the count was at the      *)
(* bound, reads the head, gives up if the list is closed, links its block *)
(* to the head it read and swaps the block in only if the head is still   *)
(* that one, else reads again. The owner, when its own cache has no       *)
(* block, swaps the whole list out at once, keeps what its cache has room *)
(* for and frees the rest. As its thread ends it swaps the list out and   *)
(* the head to closed in one step, frees all it holds, then lets the      *)
(* record go; the next thread to claim the record opens it again. A block *)
(* given up on goes back to the system.                                   *)
(*                                                                          *)
(* What is abstract: a block's memory is its place (`where`); the walk of *)
(* a list swapped out is one step, since no other thread can reach it.    *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Blocks,    \* the blocks of one class
    Foreign,   \* threads that free blocks they did not allocate
    Kept,      \* the bound of a cache and of a list
    Allocs,    \* allocations of each owner
    Mutation   \* "none", or the name of the one rule this run takes away

Null == "null"
Closed == "closed"
Owners == {"o", "n"}   \* "o" holds the record first, "n" claims it after

Min(a, b) == IF a < b THEN a ELSE b

\* The blocks reachable from `h` through `nx`.
RECURSIVE Chain(_, _, _)
Chain(h, nx, fuel) ==
    IF h \in Blocks /\ fuel > 0 THEN {h} \cup Chain(nx[h], nx, fuel - 1) ELSE {}

(* --algorithm RemoteFree
variables
    where = [b \in Blocks |-> "sys"],   \* sys, list, push, use, an owner's cache, a foreign thread
    origin = [b \in Blocks |-> Null],   \* the owner that allocated it last
    head = Null,
    next = [b \in Blocks |-> Null],
    count = 0,
    claimed = TRUE,                     \* "o" holds the record from the start
    owning = {"o"},                     \* owners whose list it is (open, not closing)
    ownersDone = 0,
    refusedClosed = FALSE,              \* (record) a push found the list closed
    refusedFull = FALSE,                \* (record) a push found the list at its bound
    reclaimed = FALSE;                  \* (record) "n" took back a block "o" allocated

define
    Held(t) == {b \in Blocks : where[b] = t}
    InList == Held("list")
end define;

fair process owner \in Owners
variables blk = Null, allocs = 0;
begin
O_claim:
    if self = "n" then
        await ~claimed;
        claimed := TRUE;
O_reset:
        head := Null;
        owning := owning \cup {self};
    end if;
O_loop:
    while allocs < Allocs do
O_take:
        if Held(self) # {} then
            with x \in Held(self) do blk := x; end with;
        elsif head \in Blocks then
            \* The list swapped out, and walked: the cache keeps what it has room for.
            with got = Chain(head, next, Cardinality(Blocks) + 1),
                 keep \in {k \in SUBSET got : Cardinality(k) = Min(Kept, Cardinality(got))} do
                head := Null;
                count := count - Cardinality(got);
                where := [x \in Blocks |->
                            IF x \in keep THEN self
                            ELSE IF x \in got THEN "sys" ELSE where[x]];
                if self = "n" /\ \E x \in keep : origin[x] = "o" then
                    reclaimed := TRUE;
                end if;
            end with;
        end if;
O_fresh:
        if blk = Null then
            if Held(self) # {} then
                with x \in Held(self) do blk := x; end with;
            elsif Held("sys") # {} then
                with x \in Held("sys") do blk := x; end with;
            end if;
        end if;
O_use:
        if blk # Null then
            where[blk] := "use";
            origin[blk] := self;
O_end:
            either
                \* The task ends on another thread.
                with f \in Foreign do where[blk] := f; end with;
            or
                \* It ends here.
                if Cardinality(Held(self)) < Kept then
                    where[blk] := self;
                else
                    where[blk] := "sys";
                end if;
            end either;
            blk := Null;
        end if;
O_next:
        allocs := allocs + 1;
    end while;
O_close:
    \* MUTATION release_first: the record goes before the list is closed.
    if Mutation = "release_first" then
        claimed := FALSE;
    end if;
O_swap:
    \* MUTATION store_close: the list is swapped out, and closed in a step of its own.
    with got = Chain(head, next, Cardinality(Blocks) + 1) do
        count := count - Cardinality(got);
        where := [x \in Blocks |-> IF x \in got THEN "sys" ELSE where[x]];
    end with;
    owning := owning \ {self};
    if Mutation = "store_close" then
        head := Null;
O_store:
        head := Closed;
    else
        head := Closed;
    end if;
O_drop:
    where := [x \in Blocks |-> IF where[x] = self THEN "sys" ELSE where[x]];
O_release:
    claimed := FALSE;
    ownersDone := ownersDone + 1;
end process;

fair process foreign \in Foreign
variables fb = Null, h = Null;
begin
F_loop:
    while TRUE do
F_wait:
        await Held(self) # {} \/ ownersDone = 2;
        if Held(self) = {} then
            goto Done;
        else
            with x \in Held(self) do fb := x; end with;
            where[fb] := "push";
        end if;
F_count:
        \* The count's old value decides at once; giving up undoes the count.
        \* MUTATION no_bound: the count is not looked at.
        count := count + 1;
        if count > Kept /\ Mutation # "no_bound" then
F_full:
            count := count - 1;
            where[fb] := "sys";
            fb := Null;
            refusedFull := TRUE;
            goto F_loop;
        end if;
F_load:
        h := head;
F_check:
        if h = Closed then
            count := count - 1;
            where[fb] := "sys";
            fb := Null;
            h := Null;
            refusedClosed := TRUE;
            goto F_loop;
        else
            next[fb] := h;
        end if;
F_cas:
        \* MUTATION blind_push: the block is swapped in whatever the head is now.
        if head = h \/ Mutation = "blind_push" then
            head := fb;
            where[fb] := "list";
            fb := Null;
            h := Null;
        else
            h := head;
            goto F_check;
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

\* The list holds exactly the blocks placed in it, while it is open.
ListConsistent == head # Closed => InList = Chain(head, next, Cardinality(Blocks) + 1)

\* A closed list holds nothing.
ClosedEmpty == head = Closed => InList = {}

\* The list never holds more than the bound.
BoundedList == Cardinality(InList) <= Kept

\* The list of an owner that holds it is open.
OpenWhileOwned == owning # {} => head # Closed

\* The count is never below zero.
CountOK == count >= 0

\* Once every thread is done, every block is back with the system.
AllFreed ==
    (\A p \in Owners \cup Foreign : pc[p] = "Done") => \A x \in Blocks : where[x] = "sys"

\* Every thread ends.
Terminates == <>(\A p \in Owners \cup Foreign : pc[p] = "Done")

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

NoRefusedClosed == ~refusedClosed
NoRefusedFull == ~refusedFull
NoReclaimed == ~reclaimed
=============================================================================

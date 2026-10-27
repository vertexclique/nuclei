------------------------------- MODULE Radix -------------------------------
(***************************************************************************)
(* A channel's radix queue (src/sync/mpsc/queue/radix.rs): leaves found    *)
(* through a tree whose root grows, pushed to by producers and taken from  *)
(* by one receiver, with kovan reclaiming what the receiver passed, at the *)
(* grain of each access.                                                    *)
(*                                                                          *)
(* In plain words: a push claims a position with a fetch-add, loads the    *)
(* root, and while the position lies past the root's span grows it by one  *)
(* level (a new root whose first child is the old one, swapped in once: a  *)
(* swap it loses was another push's growth). It then descends a level at a *)
(* time to the position's leaf, making a missing node (a swap it loses     *)
(* leaves the other push's node), writes its value and marks it ready.     *)
(* The receiver takes the value of its position once ready; done with a    *)
(* leaf, it waits until the next leaf exists, then retires the leaf it     *)
(* leaves and every inner node whose span ended with it.                   *)
(*                                                                          *)
(* kovan, abstractly: a push holds a guard for its whole push, and a node  *)
(* it loads while not yet retired is protected until the push ends; a      *)
(* retired node no guard protects may be freed at any moment (the step     *)
(* `Free`). Following a pointer reads the node it names, and that must     *)
(* never meet a freed node.                                                *)
(*                                                                          *)
(* What is abstract: a leaf holds one position, so the receiver leaves a   *)
(* leaf after every value; a node is its level and its index at that level *)
(* (`<<level, index>>`, a leaf at level 0), which the swaps keep unique; a *)
(* node a push made and could not link is freed unseen and not modelled.   *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Producers,  \* the pushers
    Pushes,     \* how many values each pushes
    Fan,        \* children per inner node
    Mutation    \* "none", or the name of the one rule this run takes away

Total == Cardinality(Producers) * Pushes
Positions == 0 .. Total - 1
\* Fan to the power `l`: the leaves a node of level `l` spans.
RECURSIVE Pow(_)
Pow(l) == IF l = 0 THEN 1 ELSE Fan * Pow(l - 1)
\* The levels a root may reach for these positions, and one more.
MaxLevel == CHOOSE l \in 1 .. Total + 1 : Pow(l) >= Total /\ \A m \in 1 .. l - 1 : Pow(m) < Total
\* The node of level `l` on the path of leaf `n`.
NodeOf(l, n) == <<l, n \div Pow(l)>>
\* The most steps a push takes from its claim to its write: a growth try and a
\* look per level, a descent step per level, and its write.
StepBound == 2 * (MaxLevel + 1) + MaxLevel + 2
ReceiverT == "receiver"
KovanT == "kovan"

(* --algorithm Radix
variables
    tail = 0,
    rootLevel = 1,                        \* the root is <<rootLevel, 0>>
    made = {<<1, 0>>},                    \* nodes linked into the tree
    ready = [p \in Positions |-> FALSE],
    taken = [p \in Positions |-> 0],
    cur = 0,                              \* the receiver's leaf (its position)
    retired = {},
    freed = {},
    prot = [p \in Producers |-> {}],
    steps = [p \in Producers |-> 0],      \* the steps of each push's current push
    touchedFreed = FALSE,                 \* (record) a node was read after it was freed
    grown = FALSE,                        \* (record) the root grew
    raced = FALSE;                        \* (record) a push found a node another made first

define
    Protect(self, k) == IF k \in retired THEN prot[self] ELSE prot[self] \cup {k}
    Touch(k) == touchedFreed \/ k \in freed
    Done == cur = Total - 1 /\ taken[Total - 1] = 1
    \* The inner nodes whose span ends with leaf `n`, and the leaf.
    EndedBy(n) == {NodeOf(0, n)} \cup
        {NodeOf(l, n) : l \in {m \in 1 .. rootLevel : (n + 1) % Pow(m) = 0}}
    Freeable == {k \in retired \ freed : \A p \in Producers : k \notin prot[p]}
end define;

fair process push \in Producers
variables n = 0, pos = 0, level = 0, seen = 0;
begin
P_loop:
    while n < Pushes do
P_claim:
        pos := tail;
        tail := tail + 1;
        steps[self] := 0;
P_root:
        \* Load the root; the guard protects it (it is never retired as the root).
        seen := rootLevel;
        prot[self] := Protect(self, <<rootLevel, 0>>);
        steps[self] := steps[self] + 1;
P_span:
        touchedFreed := Touch(<<seen, 0>>);
        if pos >= Pow(seen) then
P_grow:
            \* Swap in a root one level higher, if the root is still the one seen.
            if rootLevel = seen then
                rootLevel := seen + 1;
                made := made \cup {<<seen + 1, 0>>};
                grown := TRUE;
            end if;
            steps[self] := steps[self] + 1;
            goto P_root;
        end if;
P_start:
        level := seen;
P_descend:
        while level > 0 do
            \* Read the child slot of the node on the path; make it if missing.
            touchedFreed := Touch(NodeOf(level, pos));
            if NodeOf(level - 1, pos) \notin made then
                made := made \cup {NodeOf(level - 1, pos)};
            else
                raced := TRUE;
            end if;
P_child:
            \* Load the child (not taken from the swap): protected if not retired.
            prot[self] := Protect(self, NodeOf(level - 1, pos));
            level := level - 1;
            steps[self] := steps[self] + 1;
        end while;
P_write:
        touchedFreed := Touch(NodeOf(0, pos));
        ready[pos] := TRUE;
        steps[self] := steps[self] + 1;
P_unpin:
        prot[self] := {};
        n := n + 1;
    end while;
end process;

fair process receiver = ReceiverT
begin
R_loop:
    while ~Done do
R_take:
        \* The receiver reads its leaf, which only it retires.
        touchedFreed := Touch(NodeOf(0, cur));
        if ready[cur] /\ taken[cur] = 0 then
            taken[cur] := 1;
        end if;
R_next:
        \* Done with the leaf (its value taken): on once the next leaf exists.
        \* MUTATION leave_early: it leaves without waiting for the next leaf.
        if taken[cur] = 1 /\ cur < Total - 1
           /\ (NodeOf(0, cur + 1) \in made \/ Mutation = "leave_early") then
R_leave:
            \* MUTATION retire_parent_early: the parent goes with every leaf.
            if Mutation = "retire_parent_early" then
                retired := retired \cup EndedBy(cur) \cup {NodeOf(1, cur)};
            else
                retired := retired \cup EndedBy(cur);
            end if;
            cur := cur + 1;
        end if;
    end while;
end process;

\* kovan frees a retired node once no guard protects it, until everyone is done.
process kovan = KovanT
begin
K_free:
    while TRUE do
        either
            with k \in Freeable do
                freed := freed \cup {k};
            end with;
        or
            await Done /\ \A p \in Producers : pc[p] = "Done";
            goto K_done;
        end either;
    end while;
K_done:
    skip;
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

\* No node is read after kovan freed it.
NoFreedTouch == ~touchedFreed

\* The root is never retired.
RootLive == <<rootLevel, 0>> \notin retired

\* Every value is taken at most once, and only once it was written.
TakenOnce == \A p \in Positions : taken[p] <= 1 /\ (taken[p] = 1 => ready[p])

\* Wait-free: a push takes a bounded number of its own steps, whatever the
\* others do.
WaitFree == \A p \in Producers : steps[p] <= StepBound

\* Every value is taken.
AllTaken == <>Done

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* The root grows.
NoGrow == ~grown
\* A push finds a node another push made first.
NoRace == ~raced
\* kovan frees a node the receiver left.
NoFree == freed = {}
=============================================================================

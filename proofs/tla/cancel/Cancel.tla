------------------------------ MODULE Cancel -------------------------------
(***************************************************************************)
(* The cancellation token's tree (src/sync/cancellation.rs, node.rs), at  *)
(* the grain of its atomic steps.                                          *)
(*                                                                          *)
(* In plain words: a token is a strong handle of a node. A node has a      *)
(* flag, a strong handle of its parent, and a registry of weak handles of  *)
(* its children. `child_token` makes a node, puts it in its parent's       *)
(* registry, then reads the parent's flag, and cancels itself if it is     *)
(* set. `cancel` sets the node's flag, then walks the subtree: at each     *)
(* node it reads the registry, upgrading each weak handle (a child that    *)
(* went is skipped), marks every child it found that is not marked yet,    *)
(* and goes on into every one of them, marked before or not, letting go of *)
(* each once it has visited it. A node whose last strong handle goes is    *)
(* freed: it leaves its parent's registry and lets go of its parent, which *)
(* may free the parent in turn, in a loop up the chain.                    *)
(*                                                                          *)
(* The tree here: the root R, its child A (made by K), and A's child B     *)
(* (made by K next). X cancels R; Y, when present, cancels A at the same    *)
(* time; D holds R's first token and drops it.                             *)
(*                                                                          *)
(* What is modelled step by step: the making of a node, its insert into   *)
(* the registry, the read of the parent's flag, each flag set, each read   *)
(* of a registry with its upgrades, each strong handle let go and each     *)
(* free with its removal from the registry. What is abstract: the SeqCst   *)
(* fences (a TLA+ step is seen by every thread at once, so the model is    *)
(* the fenced protocol; that the fences give it is the loom models' and    *)
(* the module docs' part); the registry's read is one step, where kovan's *)
(* walk takes many, which the module docs' argument covers (an entry       *)
(* present for the whole walk is yielded); the flag's waking of waiters is *)
(* the event's.                                                            *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, Sequences, TLC

CONSTANTS
    WithY,     \* a second canceller cancels A while X cancels R
    DropAll,   \* every token is dropped at the end: the whole tree must be freed
    Mutation   \* "none", or the name of the one rule this run takes away

R == 1
A == 2
B == 3
None == 0
Nodes == {R, A, B}
Parent == [n \in Nodes |-> CASE n = A -> R [] n = B -> A [] OTHER -> None]
\* The descendants of `n`.
Desc(n) == CASE n = R -> {A, B} [] n = A -> {B} [] OTHER -> {}
K == 10
X == 11
Y == 12
D == 13
Cancellers == {X} \cup (IF WithY THEN {Y} ELSE {})

(* --algorithm Cancel
variables
    exists = [n \in Nodes |-> n = R],
    created = [n \in Nodes |-> n = R],  \* its child_token returned
    \* Strong handles: tokens, children's handles of their parent, and the
    \* handles a walk holds. R starts with D's token and X's clone.
    count = [n \in Nodes |-> IF n = R THEN 2 ELSE 0],
    freed = [n \in Nodes |-> FALSE],
    reg = [n \in Nodes |-> {}],
    flag = [n \in Nodes |-> FALSE],
    \* records
    starts = [p \in Cancellers |-> {}],      \* descendants whose child_token had returned as the call began
    cancelDone = [p \in Cancellers |-> FALSE],
    touchedFreed = FALSE,   \* a walk read or marked a freed node
    selfCancelled = FALSE,  \* a new child found its parent cancelled
    foundMarked = FALSE,    \* a walk met a child another call had marked
    yHasA = FALSE;          \* K handed Y a clone of its token of A

define
    \* When a cancel returns, every descendant that existed as it began is
    \* cancelled, or gone (freed: nobody can look at it any more).
    CancelReturnsWithSubtree ==
        \A p \in Cancellers : cancelDone[p] => \A d \in starts[p] : flag[d] \/ freed[d]
    \* A child made at the same time as X's cancel is cancelled by the time
    \* both returned, or gone.
    RacingChildCancelled ==
        \A d \in {A, B} : cancelDone[X] /\ created[d] => flag[d] \/ freed[d]
    NoTouchFreed == ~touchedFreed
    \* At the end, a freed node is in no registry, and with every token gone
    \* every node is freed.
    AllDone == \A p \in DOMAIN pc : pc[p] = "Done"
    RegistryHoldsTheLive == AllDone => \A n \in Nodes : \A c \in reg[n] : ~freed[c]
    NoLeak == AllDone /\ DropAll => \A n \in Nodes : freed[n]
    \* Witnesses.
    NoSelfCancel == ~selfCancelled
    NoFoundMarked == ~foundMarked
end define;

\* Lets go of one strong handle of `node`; a node freed by it leaves its
\* parent's registry and lets go of its parent, up the chain.
procedure release(node = None)
variables cur = None;
begin
Rel_start:
    cur := node;
Rel_loop:
    while cur # None do
        count[cur] := count[cur] - 1;
        if count[cur] = 0 then
            freed[cur] := TRUE;
Rel_detach:
            if Parent[cur] # None /\ Mutation # "no_detach" then
                reg[Parent[cur]] := reg[Parent[cur]] \ {cur};
            end if;
            if Mutation = "no_walk_up" then
                cur := None;
            else
                cur := Parent[cur];
            end if;
        else
            cur := None;
        end if;
    end while;
    return;
end procedure;

\* Cancels `this` and its subtree.
procedure cancel(this = None)
variables pending = {}, found = {}, newly = {}, skipped = {}, visiting = None, rel = None;
begin
Ca_set:
    if self \in Cancellers then
        starts[self] := {d \in Desc(this) : created[d]};
    end if;
    flag[this] := TRUE;
    visiting := this;
Ca_collect:
    \* The registry walk, after the fence: each live child upgraded.
    touchedFreed := touchedFreed \/ freed[visiting];
    if Mutation = "visit_without_upgrade" then
        found := reg[visiting];
    else
        found := {c \in reg[visiting] : count[c] > 0};
        count := [n \in Nodes |-> IF n \in found THEN count[n] + 1 ELSE count[n]];
    end if;
Ca_mark:
    newly := {c \in found : ~flag[c]};
    touchedFreed := touchedFreed \/ \E c \in found : freed[c];
    foundMarked := foundMarked \/ found # newly;
    flag := [n \in Nodes |-> IF n \in found THEN TRUE ELSE flag[n]];
    if Mutation = "skip_marked" then
        pending := pending \cup newly;
        skipped := found \ newly;
    else
        pending := pending \cup found;
    end if;
Ca_dropSkipped:
    while skipped # {} do
        rel := CHOOSE c \in skipped : TRUE;
        skipped := skipped \ {rel};
        call release(rel);
    end while;
Ca_doneVisit:
    \* The visited node's handle goes (the walk's own, not the caller's).
    if visiting # this /\ Mutation # "visit_without_upgrade" then
        call release(visiting);
    end if;
Ca_pop:
    if pending # {} then
        visiting := CHOOSE c \in pending : TRUE;
        pending := pending \ {visiting};
        goto Ca_collect;
    end if;
Ca_return:
    return;
end procedure;

\* Makes `new` a child of `par`.
procedure child(par = None, new = None)
variables parSet = FALSE;
begin
Ch_make:
    exists[new] := TRUE;
    \* Its token, and its handle of its parent.
    count := [count EXCEPT ![new] = 1, ![par] = @ + 1];
    if Mutation = "flag_before_insert" then
        goto Ch_readFirst;
    end if;
Ch_insert:
    reg[par] := reg[par] \cup {new};
Ch_read:
    \* After the fence.
    parSet := flag[par];
    goto Ch_decide;
Ch_readFirst:
    parSet := flag[par];
Ch_insertLate:
    reg[par] := reg[par] \cup {new};
Ch_decide:
    if parSet then
        selfCancelled := TRUE;
        call cancel(new);
    end if;
Ch_done:
    created[new] := TRUE;
    return;
end procedure;

fair process creator = K
begin
K_a:
    call child(R, A);
K_giveY:
    \* Y's clone of A's token, made while K holds it.
    if WithY then
        count[A] := count[A] + 1;
        yHasA := TRUE;
    end if;
K_b:
    call child(A, B);
K_dropB:
    if DropAll then
        call release(B);
    end if;
K_dropA:
    if DropAll then
        call release(A);
    end if;
end process;

fair process cancelRoot = X
begin
X_cancel:
    call cancel(R);
X_returned:
    cancelDone[X] := TRUE;
X_drop:
    if DropAll then
        call release(R);
    end if;
end process;

fair process cancelMiddle \in (IF WithY THEN {Y} ELSE {})
begin
Y_clone:
    await yHasA;
Y_cancel:
    call cancel(A);
Y_returned:
    cancelDone[Y] := TRUE;
Y_drop:
    if DropAll then
        call release(A);
    end if;
end process;

fair process dropRoot = D
begin
D_drop:
    if DropAll then
        call release(R);
    end if;
end process;
end algorithm; *)
=============================================================================

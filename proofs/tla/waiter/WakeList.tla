------------------------------- MODULE WakeList -------------------------------
(***************************************************************************)
(* A drain pass's wake list (src/sync/waiter/wake.rs: `WakeList::grant`,   *)
(* `WakeList::nudge`, `WakeList::push`, `WakeList::pop`, `Drop for         *)
(* WakeList`, `WakeRest`), walked once the pass gave up the drain role.    *)
(*                                                                          *)
(* In plain words: a pass that granted or nudged waiters whose wakers it   *)
(* claimed linked those nodes into its wake list, each with a wake         *)
(* reference. Once the pass is done the list is dropped, and its drop pops *)
(* the nodes in order: it reads the node's link to the next one, then      *)
(* takes the claimed waker and lets go of its wake reference (the last     *)
(* reference out puts the node back in its pool), then calls the waker.  *)
(* A waker can panic. While a call runs, a `WakeRest` guard sits on the    *)
(* stack: unwinding drops it, and its drop wakes every node still in the  *)
(* list, so the panic strands no waiter behind the one that panicked.      *)
(*                                                                          *)
(* Who acts: the wake list, and each node's future, which may finish and   *)
(* let go of its own reference at any moment; the pool hands a freed node  *)
(* to a new future, which writes its link. What must never happen: the    *)
(* list reads a node after it let go of it while someone else held it (the *)
(* node may be freed and reused); a node in the list whose waker is never  *)
(* called.                                                                 *)
(*                                                                          *)
(* What is abstract: the node's state word and its two slots (the waiter   *)
(* model checks them, a waker claimed again during its take included); a   *)
(* waker is called once; a second panic while unwinding aborts the        *)
(* process, so at most one waker panics.                                   *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    K,         \* the nodes the pass linked into its wake list, in order 1..K
    PanicAt,   \* the node whose waker panics (0: none)
    Mutation   \* "none", or the name of the one rule this run takes away

Nodes == 1..K
ListT == "list"
\* The end of the list.
Nil == 0

(* --algorithm WakeList
variables
    link = [n \in Nodes |-> IF n < K THEN n + 1 ELSE Nil], \* `wake_next`
    refs = [n \in Nodes |-> {"W", "F"}],  \* the wake list's and the future's references
    freed = [n \in Nodes |-> FALSE],      \* the node went back to the pool
    reused = [n \in Nodes |-> FALSE],     \* a new future took it and wrote its link
    woken = [n \in Nodes |-> FALSE],      \* the node's waker was called
    stale = FALSE,                        \* (record) the list read a node it let go of
    panicked = FALSE;                     \* (record) a waker panicked

define
    \* The list never touches a node it let go of while another party held it.
    NoStaleAccess == ~stale
    \* Once the walk is over, every node in the list was woken, panic or not.
    AllWoken == pc[ListT] = "Done" => \A n \in Nodes : woken[n]
end define;

\* `release`: one reference goes; the last one out puts the node in the pool.
macro release(n, bit) begin
    if refs[n] = {bit} then
        refs[n] := {};
        freed[n] := TRUE;
    else
        refs[n] := refs[n] \ {bit};
    end if;
end macro;

\* The wake list's drop, and `WakeRest`'s once a waker panicked.
fair process list = ListT
variables head = 1, nd = Nil, nxt = Nil, last = FALSE;
begin
L_pop:
    \* `pop`: the front node, or the end of the walk.
    if head = Nil then
        goto Done;
    else
        nd := head;
    end if;
L_link:
    \* The link is read while the list still holds the node.
    \* MUTATION link_after_take: it is read after the take let the node go.
    if Mutation # "link_after_take" then
        nxt := link[nd];
    end if;
L_take:
    \* `take_claimed`: the waker taken, the wake reference let go; the last one out puts
    \* the node back in the pool.
    last := refs[nd] = {"W"};
    release(nd, "W");
L_link_late:
    if Mutation = "link_after_take" then
        stale := stale \/ (~last /\ (freed[nd] \/ reused[nd]));
        nxt := IF reused[nd] THEN Nil ELSE link[nd];
    end if;
L_wake:
    \* The waker is called. A panic unwinds into `WakeRest`'s drop, which goes on with the
    \* rest of the list.
    \* MUTATION no_rest: no guard sits on the stack; the panic leaves the rest unwoken.
    woken[nd] := TRUE;
    head := nxt;
    if nd = PanicAt then
        panicked := TRUE;
        if Mutation = "no_rest" then
            goto Done;
        end if;
    end if;
L_next:
    goto L_pop;
end process;

\* Each node's future: its node was granted or nudged, so any poll of it (the wake, or a
\* poll by something else) may see that, finish and let go of its reference, before or
\* after the list wakes it. The pool may then hand the freed node to a new future, which
\* writes its link.
fair process fut \in Nodes
begin
F_release:
    release(self, "F");
F_reuse:
    await freed[self];
    either
        reused[self] := TRUE;
        link[self] := Nil;
    or
        skip;
    end either;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

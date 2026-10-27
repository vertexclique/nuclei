------------------------------ MODULE Segments ------------------------------
(***************************************************************************)
(* A channel's message queue (src/sync/mpsc/queue.rs): segments of cells,  *)
(* pushed to by any number of producers and taken from by one receiver,    *)
(* with kovan reclaiming the segments the receiver leaves, at the grain of *)
(* each atomic access.                                                      *)
(*                                                                          *)
(* In plain words: a push loads the tail's segment pointer, then claims a  *)
(* position with a fetch-add on the tail's word, and walks from that       *)
(* segment to the one of its position, linking a successor where the list *)
(* ends. Before it steps onto a successor it looks at the start the        *)
(* receiver published for its head segment: a successor the receiver       *)
(* passed may be retired, so the walk starts again from the head segment   *)
(* the receiver published. It moves the tail's pointer to its segment if   *)
(* the pointer still names the one it began at, writes its value and      *)
(* marks the cell ready. The receiver takes the cell of its position once  *)
(* ready, and leaves a used-up segment once its successor is linked: it    *)
(* publishes the successor, then its start, moves the tail's pointer off   *)
(* the segment it leaves if it is on it, and retires that segment.         *)
(*                                                                          *)
(* kovan, abstractly: a push holds a guard for its whole walk, and a       *)
(* segment it loads while not yet retired is protected by it until its     *)
(* push ends; a retired segment that no guard protects may be freed at any *)
(* moment (the environment's step `Free`). Loading a pointer reads no      *)
(* segment; following one (reading the segment's link or a cell) does,    *)
(* and that is what must never meet a freed segment.                       *)
(*                                                                          *)
(* Segments are numbered by their place in the list: segment `k` starts at *)
(* position `k * Seg`, and its successor is `k + 1`. A push's spare        *)
(* segment that loses the race to link is freed by it unseen, so it is not *)
(* modelled.                                                                *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Producers,  \* the pushers
    Pushes,     \* how many values each pushes
    Seg,        \* cells per segment
    Mutation    \* "none", or the name of the one rule this run takes away

Total == Cardinality(Producers) * Pushes
Positions == 0 .. Total - 1
\* The segments any position needs, and one more a walk may link.
Segs == 0 .. (Total - 1) \div Seg + 1
SegOf(pos) == pos \div Seg
ReceiverT == "receiver"
KovanT == "kovan"

(* --algorithm Segments
variables
    tailPos = 0,                         \* the next position a push claims
    tailSeg = 0,                         \* the tail's segment pointer
    linked = [k \in Segs |-> k = 0],     \* the segment is in the list
    ready = [p \in Positions |-> FALSE], \* the position's value is written
    taken = [p \in Positions |-> 0],     \* how often the receiver took it
    headPos = 0,                         \* the receiver's next position
    headSeg = 0,                         \* the receiver's segment
    published = 0,                       \* the head segment, published
    headStart = 0,                       \* its start, as a segment number
    retired = {},
    freed = {},
    prot = [p \in Producers |-> {}],     \* the segments each push's guard protects
    touchedFreed = FALSE,                \* (record) a segment was read after it was freed
    restarted = FALSE,                   \* (record) a walk started again
    loadedRetired = FALSE;               \* (record) a walk loaded a successor already retired

define
    \* Loading `k` now: protected if it is not retired yet.
    Protect(self, k) == IF k \in retired THEN prot[self] ELSE prot[self] \cup {k}
    \* Reading segment `k`: records it if `k` was freed already.
    Touch(k) == touchedFreed \/ k \in freed
    Done == headPos = Total
    \* Retired, and protected by no guard: kovan may free it.
    Freeable == {k \in retired \ freed : \A p \in Producers : k \notin prot[p]}
end define;

fair process push \in Producers
variables n = 0, first = 0, seg = 0, pos = 0, target = 0;
begin
P_loop:
    while n < Pushes do
P_load_first:
        \* MUTATION load_after_claim: the walk's start is loaded after the claim.
        if Mutation # "load_after_claim" then
            first := tailSeg;
            prot[self] := Protect(self, tailSeg);
        end if;
P_claim:
        pos := tailPos;
        tailPos := tailPos + 1;
        target := SegOf(pos);
P_load_late:
        if Mutation = "load_after_claim" then
            first := tailSeg;
            prot[self] := Protect(self, tailSeg);
        end if;
        seg := first;
P_walk:
        while seg # target do
P_link:
            \* Reads the segment's link, linking a successor where the list ends.
            touchedFreed := Touch(seg);
            if ~linked[seg + 1] then
                linked[seg + 1] := TRUE;
            end if;
P_load_next:
            \* The successor loaded out of `seg`: protected only if not retired yet.
            touchedFreed := Touch(seg);
            loadedRetired := loadedRetired \/ (seg + 1) \in retired;
            prot[self] := Protect(self, seg + 1);
P_check:
            \* MUTATION no_check: the walk steps on without looking at the head's start.
            if headStart > seg + 1 /\ Mutation # "no_check" then
                restarted := TRUE;
                \* MUTATION restart_from_tail: the walk starts again from the tail's pointer.
                if Mutation = "restart_from_tail" then
                    seg := tailSeg;
                    prot[self] := Protect(self, tailSeg);
                else
                    seg := published;
                    prot[self] := Protect(self, published);
                end if;
            else
                seg := seg + 1;
            end if;
        end while;
P_tail:
        \* The tail's pointer moves on to this segment if it still names the start.
        if seg # first /\ tailSeg = first then
            tailSeg := seg;
        end if;
P_write:
        touchedFreed := Touch(seg);
        ready[pos] := TRUE;
P_unpin:
        prot[self] := {};
        n := n + 1;
    end while;
end process;

fair process receiver = ReceiverT
variables old = 0;
begin
R_loop:
    while ~Done do
R_look:
        touchedFreed := Touch(headSeg);
        if headPos = (headSeg + 1) * Seg then
            \* The head segment is used up: on once its successor is linked.
            if linked[headSeg + 1] then
                old := headSeg;
                goto R_retire_first;
            else
                goto R_loop;
            end if;
        else
            if ready[headPos] then
                taken[headPos] := taken[headPos] + 1;
                headPos := headPos + 1;
            end if;
            goto R_loop;
        end if;
R_retire_first:
        \* MUTATION retire_first: the segment is retired before the start is published.
        if Mutation = "retire_first" then
            retired := retired \cup {old};
        end if;
R_publish:
        published := old + 1;
R_start:
        headStart := old + 1;
R_tail:
        \* MUTATION no_tail_move: the tail's pointer is left on the segment.
        if tailSeg = old /\ Mutation # "no_tail_move" then
            tailSeg := old + 1;
        end if;
R_retire:
        if Mutation # "retire_first" then
            retired := retired \cup {old};
        end if;
        headSeg := old + 1;
    end while;
end process;

\* kovan frees a retired segment once no guard protects it, at any moment,
\* until every push and the receiver are done.
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

\* No segment is read after kovan freed it.
NoFreedTouch == ~touchedFreed

\* The tail's pointer never names a retired segment: a walk that starts from
\* it may read it.
TailLive == tailSeg \notin retired

\* A walk never passes the segment of its position.
WalkInBounds == \A p \in Producers :
    pc[p] \in {"P_walk", "P_link", "P_load_next", "P_check"} => seg[p] <= target[p]

\* Every value is taken at most once, and only once it was written.
TakenOnce == \A p \in Positions : taken[p] <= 1 /\ (taken[p] = 1 => ready[p])

\* A retired segment holds no value still to be taken, and no push writes into it.
RetiredEmpty == \A k \in retired :
    /\ \A p \in Positions : SegOf(p) = k => taken[p] = 1
    /\ \A q \in Producers : pc[q] \in {"P_tail", "P_write"} => seg[q] # k

\* Every value is taken.
AllTaken == <>Done

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A walk starts again from the head segment.
NoRestart == ~restarted
\* A walk loads a successor that is retired already (the check then stops it).
NoRetiredLoad == ~loadedRetired
\* kovan frees a segment.
NoFree == freed = {}
=============================================================================

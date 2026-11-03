------------------------------ MODULE Claim ------------------------------
(***************************************************************************)
(* Who takes a biased side as its owner (src/io/side.rs: `Side::role`,     *)
(* with its `own == 0` look, and `Side::claim`, the compare-and-swap from  *)
(* zero; src/io/source/hooks.rs: `SourceHooks::owner`, the home check).    *)
(*                                                                          *)
(* Who acts. Threads poll one side of a source: the thread of the shard    *)
(* the source is homed at, the thread of another shard, and a thread that  *)
(* runs no shard. The source is not homed at first; its registration       *)
(* homes it at the first shard in the end.                                 *)
(*                                                                          *)
(* What can happen. A thread's candidate is the shard it runs, if the      *)
(* source is homed there or not homed yet, else none (`owner`). A side     *)
(* claimed by a candidate is that thread's to take as its owner; a side    *)
(* claimed by nobody is claimed by the first candidate whose               *)
(* compare-and-swap from zero wins (`claim`). The owner takes the side      *)
(* biased, with no locked instruction, only while it does not hold it      *)
(* already: a call it makes under its own hold is a foreign one, which    *)
(* leaves its work for that hold's release. Every other thread takes the   *)
(* side as a foreign one: it raises `intent`, and if the owner holds it, it *)
(* leaves its work; else the side goes shared for good and the thread      *)
(* takes it through the word, as the owner then does too.                  *)
(*                                                                          *)
(* What must never happen. Two holds of the side at once: two owners, or   *)
(* an owner holding biased twice (`Exclusive`). A shard that is not the     *)
(* source's home claims the side once the source is homed (`HomeClaims`).  *)
(*                                                                          *)
(* What is abstract. Memory is sequentially consistent: the store buffers *)
(* and the heavy fence are SideBiased.tla's, which checks the biased      *)
(* protocol for one owner; this model checks who that owner is. A take    *)
(* that leaves its work is done; a hold is a step, and a nested call is   *)
(* made once by the owner under its own hold.                              *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC                                            \* numbers, set sizes, TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Mutation   \* "none", or the name of the one rule this run takes away

Home == 1                                                                    \* the thread of the shard the source is homed at
Other == 2                                                                   \* the thread of another shard
None == 3                                                                    \* a thread that runs no shard
Threads == {Home, Other, None}                                               \* the threads that poll the side

(* --algorithm Claim
variables
    homed = FALSE,          \* the source is homed at Home's shard (`reg.home`)
    claimed = 0,            \* the side's claim: the thread whose shard claimed it; 0: none (`Side::owner`)
    ownBy = {},             \* the threads holding the side biased (`own`): a thread's own look sees its own store only
    shared = FALSE,         \* the side went shared, for good
    word = 0,               \* the thread that holds the side through its word; 0: nobody
    holds = 0,              \* holds of the side at this moment
    lateClaim = FALSE;      \* (record) a shard claimed the side after the source was homed elsewhere

define
    \* The shard a thread is a candidate for (`SourceHooks::owner`): the one it runs, while the source is
    \* homed there or not homed yet; else none.
    \* MUTATION home_unchecked: any shard's thread is a candidate, wherever the source is homed.
    Candidate(t) == IF t = None THEN 0
                    ELSE IF ~homed \/ t = Home \/ Mutation = "home_unchecked" THEN t
                    ELSE 0
    \* Whether thread `t` takes the side as its owner (`Side::role`): the claim is its own, and it does not
    \* hold the side already (a plain load of `own`, which shows a thread its own store, not another's).
    \* MUTATION owner_any: a candidate is the owner whoever claimed the side.
    \* MUTATION nested_as_owner: a call under the owner's own hold takes the side as its owner again.
    Owns(t) == /\ Candidate(t) # 0
               /\ (claimed = Candidate(t) \/ Mutation = "owner_any")
               /\ (t \notin ownBy \/ Mutation = "nested_as_owner")
end define;

\* The source's registration homes it at Home's shard, once.
fair process registrar = 0
begin
G_home:
    homed := TRUE;
end process;

\* Each thread polls the side twice; Home's owner makes one call nested under its own hold.
fair process poller \in Threads
variables n = 0;                                                             \* polls made so far
begin
T_loop:
    while n < 2 do
T_claim:
        \* `claim`: a candidate claims a side no shard claimed, with a compare-and-swap from zero.
        \* MUTATION claim_store: the claim is a plain store, so a second candidate overwrites the first.
        if Candidate(self) # 0 /\ ~shared /\ (claimed = 0 \/ (Mutation = "claim_store" /\ claimed # Candidate(self))) then
            claimed := Candidate(self);
            if homed /\ Candidate(self) # Home then lateClaim := TRUE; end if;
        end if;
T_take:
        if ~shared /\ Owns(self) then                                        \* the owner, biased:
            ownBy := ownBy \cup {self};                                      \* `own := 1`
            holds := holds + 1;                                              \* it holds the side
T_nested:
            \* A call under its own hold, once: its role, again.
            if self = Home /\ n = 0 then
                if Owns(self) then                                           \* (mutated) as the owner again:
                    holds := holds + 1;                                      \* a second hold
T_unnest:
                    holds := holds - 1;
                end if;                                                      \* else foreign: own is up, so it leaves its work
            end if;
T_release:
            ownBy := ownBy \ {self};                                         \* `own := 0`
            holds := holds - 1;                                              \* let go
        else                                                                 \* a foreign take, or shared mode:
            \* intent up, the heavy fence, then `own`, which the fence makes this thread see (one step here).
            if ownBy # {} /\ ~shared then                                    \* the owner holds: the work is left for it
                skip;
            else
                shared := TRUE;                                              \* shared for good (a foreign take switches it)
T_word:
                await word = 0;                                              \* the word is free:
                word := self;                                                \* taken through the word
                holds := holds + 1;
T_letgo:
                word := 0;                                                   \* let go
                holds := holds - 1;
            end if;
        end if;
T_next:
        n := n + 1;
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

\* One hold of the side at a time.
Exclusive == holds <= 1

\* Once the source is homed, no shard but its home claims the side.
HomeClaims == ~lateClaim

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* Another shard claims the side before the source is homed, and keeps it (the claim stays with the first).
NoEarlyClaim == claimed # Other
=============================================================================

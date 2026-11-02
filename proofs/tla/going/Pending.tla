------------------------------ MODULE Pending -------------------------------
(***************************************************************************)
(* A thread's own shard that leaves while the attach of one of its         *)
(* sources is still a request another thread will perform.                 *)
(*                                                                          *)
(* The Rust it models: `Shard::leave`, `let_go_of_sources`, `evict_all`    *)
(* and `wait_for_evictions` (src/rt/shard/going.rs); `Source::attach`,     *)
(* `perform`, `attach_now`, `name_ring_attach`, `attach_asked_of`,         *)
(* `evict_now` and `abandon_ring_attach` (src/io/source/requests.rs);      *)
(* `attach_ring` and `ring_attach_failed` (src/io/source/ring.rs);         *)
(* `registered_with` (src/io/source.rs); the table, `Tokens::insert`,      *)
(* `remove` and `drain_tokens` (src/rt/sources.rs); and `apply_io_ops`     *)
(* (src/rt/shard.rs).                                                      *)
(*                                                                          *)
(* In plain words: shard A, a thread's own, registered source S under a    *)
(* token (its table's slot and the slot's generation) and asked for its    *)
(* attach while another thread, the holder H, held a side of S: the        *)
(* attach is left to H, which performs it as it lets go. Then A's          *)
(* block_on returns and A leaves: it empties its table, asks for S's       *)
(* eviction (left to H as well, while H holds), and waits until S is no    *)
(* longer registered with it; then it applies its mailbox once and is      *)
(* away until its thread's next block_on. There, the first poll registers  *)
(* another source S2 in the table, and the mailbox is applied again. H     *)
(* performs what was asked in the code's order: the attach (on a ring,     *)
(* off A's thread, it posts the file's install to A's mailbox with a lease *)
(* that keeps S's descriptor open; on a readiness poller or a port it      *)
(* registers S itself; it may fail, the handle closed first, and then it   *)
(* posts a release of the token to A), then the eviction (a ring attach    *)
(* under way is abandoned, which posts a release of the token to A). A     *)
(* registration names its shard and its token, and its end clears them    *)
(* only after its posts; A's wait looks at the attach still asked of it    *)
(* first, then at the shard and token named, so it sees the attach         *)
(* throughout on a ring, which names its shard before it takes the ask.    *)
(* A drain of the table lets the generations go on, so a stale token names *)
(* nothing in it again.                                                    *)
(*                                                                          *)
(* What must never happen: a release that came late removing S2's          *)
(* registration (NoForeignRelease), and a lease waiting in A's mailbox     *)
(* while A is away, which keeps S's descriptor open for as long            *)
(* (NoLeaseLeft); and everything must end (AllEnd).                       *)
(*                                                                          *)
(* What is abstract: the sides' guard and their requests (Requests.tla:    *)
(* whoever holds both performs), the ring's install of a file applied     *)
(* while A still runs (Attach.tla), and A's slots beyond the one S and S2  *)
(* take in turn.                                                           *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Early,     \* the attach names its shard before it takes the ask (a ring); else only once registered
    Fails,     \* the attach fails: the handle closed before the holder performed it
    Mutation   \* "none", or the name of the one rule this run takes away

VARIABLES
    apc,       \* A's place: "drain" "evict" "waitAsk" "waitReg" "look" "away" "apply2" "done"
    hpc,       \* the holder's place: "hold" "pub" "take" "attach" "clear" "next" "evict" "gone" "end" "done"
    slotGen,   \* the generation of A's slot: S's, then S2's
    slotOf,    \* what the slot holds: "S", "S2" or "none"
    ask,       \* the attach asked of A and not taken yet: "A" or "none"
    reqAttach, \* the attach's request bit is raised
    reqEvict,  \* the eviction's request bit is raised
    regS,      \* S's registration: "registering" "registered" "unreg"
    home,      \* the shard S's registration names: "A" or "none"
    token,     \* the token it names: a generation, or -1 for none
    mail,      \* what waits in A's mailbox: records of a kind ("install" or "release") and a token
    held,      \* the holder holds a side (both, while it performs)
    bits,      \* the requests the holder took for this perform
    foreign    \* (record) a release removed S2's registration

vars == <<apc, hpc, slotGen, slotOf, ask, reqAttach, reqEvict, regS, home, token, mail, held,
          bits, foreign>>   \* every variable

TokS == 0                  \* S's token: the slot's first generation

\* An install of S's file in A's ring, posted with a lease that keeps S's descriptor open.
Install == [kind |-> "install", tok |-> TokS]
\* A release of S's token, freeing its slot in A's table.
Release == [kind |-> "release", tok |-> TokS]

\* A applies what waits in its mailbox (`apply_io_ops`): a release removes what the slot holds
\* under its token (a stale token names nothing); an install whose registration is gone only lets
\* go of its lease.
ApplyMail ==
    LET hit == \E m \in mail : m.kind = "release" /\ slotOf # "none" /\ slotGen = m.tok
    IN /\ mail' = {}                                          \* every op is applied
       /\ slotOf' = IF hit THEN "none" ELSE slotOf            \* the slot empties on a hit
       /\ slotGen' = IF hit THEN slotGen + 1 ELSE slotGen     \* and its generation goes on
       /\ foreign' = (foreign \/ (hit /\ slotOf = "S2"))      \* (record) S2's was removed

Init ==
    /\ apc = "drain"            \* A's block_on returned: A leaves
    /\ hpc = "hold"             \* the holder holds a side of S
    /\ slotGen = TokS           \* S's slot, at its first generation
    /\ slotOf = "S"             \* holds S
    /\ ask = "A"                \* S's attach asked of A, left to the holder
    /\ reqAttach = TRUE         \* its bit raised
    /\ reqEvict = FALSE         \* no eviction asked yet
    /\ regS = "registering"     \* S's registration is under way
    /\ home = "none"            \* naming no shard yet
    /\ token = -1               \* and no token
    /\ mail = {}                \* A's mailbox is empty
    /\ held = TRUE              \* the holder holds
    /\ bits = {}                \* and took nothing yet
    /\ foreign = FALSE          \* no release removed S2's

(***************************************************************************)
(* Shard A, on its own thread.                                             *)
(***************************************************************************)

\* `let_go_of_sources`: the table is emptied. Its generations go on past every token it handed out.
\* MUTATION gen_reset: a drain starts the generations over (the code before), so a token handed
\* out before names the next registration in the slot.
ADrain ==
    /\ apc = "drain"                                          \* A leaves
    /\ slotOf' = "none"                                       \* the table is emptied
    /\ slotGen' = IF Mutation = "gen_reset" THEN TokS ELSE slotGen + 1  \* past every token, or over
    /\ apc' = "evict"                                         \* then the eviction
    /\ UNCHANGED <<hpc, ask, reqAttach, reqEvict, regS, home, token, mail, held, bits, foreign>>

\* `Source::evict`: left to the holder while it holds; else A performs it now, on its own thread,
\* where the ring's detach and the slot's release need no post.
AEvict ==
    /\ apc = "evict"                                          \* A asks for the eviction
    /\ IF held                                                \* a side is held
       THEN /\ reqEvict' = TRUE                               \* the bit waits for the holder
            /\ UNCHANGED <<regS, home, token>>
       ELSE /\ regS' = "unreg"                                \* A ends the registration itself
            /\ home' = "none"                                 \* naming no shard
            /\ token' = -1                                    \* and no token
            /\ UNCHANGED reqEvict
    /\ apc' = "waitAsk"                                       \* then A waits
    /\ UNCHANGED <<hpc, slotGen, slotOf, ask, reqAttach, mail, held, bits, foreign>>

\* `wait_for_evictions`, first look: an attach still asked of A is waited for.
\* MUTATION no_attach_wait: A does not look at the attach still asked of it (the code before).
AWaitAsk ==
    /\ apc = "waitAsk"                                        \* A waits
    /\ ask # "A" \/ Mutation = "no_attach_wait"               \* until no attach is asked of it
    /\ apc' = "waitReg"                                       \* then it looks at the registration
    /\ UNCHANGED <<hpc, slotGen, slotOf, ask, reqAttach, reqEvict, regS, home, token, mail, held,
                   bits, foreign>>

\* `wait_for_evictions`, second look: S still registered with A under S's token is waited for.
AWaitReg ==
    /\ apc = "waitReg"                                        \* the ask was taken
    /\ apc' = IF home = "A" /\ token = TokS                   \* still registered with A
              THEN "waitAsk"                                  \* look again
              ELSE "look"                                     \* gone: on to the mailbox
    /\ UNCHANGED <<hpc, slotGen, slotOf, ask, reqAttach, reqEvict, regS, home, token, mail, held,
                   bits, foreign>>

\* `let_go_of_the_rest`: A applies its mailbox once, and is away until its next block_on.
ALook ==
    /\ apc = "look"                                           \* the evictions ended
    /\ ApplyMail                                              \* what waits is applied
    /\ apc' = "away"                                          \* A leaves
    /\ UNCHANGED <<hpc, ask, reqAttach, reqEvict, regS, home, token, held, bits>>

\* A's next block_on: its first poll registers S2 in the slot, under the slot's generation.
AAgain ==
    /\ apc = "away"                                           \* A is away
    /\ slotOf' = "S2"                                         \* S2 takes the slot
    /\ apc' = "apply2"                                        \* then the loop applies the mailbox
    /\ UNCHANGED <<hpc, slotGen, ask, reqAttach, reqEvict, regS, home, token, mail, held, bits,
                   foreign>>

\* The loop applies what waited in the mailbox meanwhile.
AApply ==
    /\ apc = "apply2"                                         \* S2 is registered
    /\ ApplyMail                                              \* the mailbox is applied
    /\ apc' = "done"                                          \* A runs on
    /\ UNCHANGED <<hpc, ask, reqAttach, reqEvict, regS, home, token, held, bits>>

(***************************************************************************)
(* The holder H, on another thread: it performs as it lets go.             *)
(***************************************************************************)

\* Holding both sides, the holder takes every request bit raised (`perform`'s swap).
\* MUTATION take_first: the attach takes the ask before it names its shard and token.
HTake ==
    /\ hpc = "hold"                                           \* the holder lets go of its side
    /\ held                                                   \* and holds both now
    /\ bits' = {r \in {"attach", "evict"} :                   \* the bits raised
                  (r = "attach" /\ reqAttach) \/ (r = "evict" /\ reqEvict)}
    /\ reqAttach' = FALSE                                     \* taken
    /\ reqEvict' = FALSE                                      \* taken
    /\ hpc' = IF ~reqAttach THEN "next"                       \* no attach: on to the eviction
              ELSE IF Early /\ Mutation # "take_first" THEN "pub"  \* a ring names its shard first
              ELSE "take"                                     \* else the ask is taken first
    /\ UNCHANGED <<apc, slotGen, slotOf, ask, regS, home, token, mail, held, foreign>>

\* `attach_now` on a ring: the registration names A and S's token before the ask goes.
HPub ==
    /\ hpc = "pub"                                            \* the attach begins
    /\ home' = "A"                                            \* it names A
    /\ token' = TokS                                          \* and S's token
    /\ hpc' = IF Mutation = "take_first" THEN "attach" ELSE "take"  \* the ask next (or already)
    /\ UNCHANGED <<apc, slotGen, slotOf, ask, reqAttach, reqEvict, regS, mail, held, bits,
                   foreign>>

\* The attach takes the ask (`Ask::attach`'s swap).
HTakeAsk ==
    /\ hpc = "take"                                           \* about to take the ask
    /\ ask' = "none"                                          \* nothing is asked of A any more
    /\ hpc' = IF Early /\ Mutation = "take_first" THEN "pub" ELSE "attach"  \* the bug names later
    /\ UNCHANGED <<apc, slotGen, slotOf, reqAttach, reqEvict, regS, home, token, mail, held, bits,
                   foreign>>

\* The attach itself. Failed (the handle closed): the registration goes back to none, and a
\* release of the token is posted to A. On a ring, off A's thread: the install goes to A's mailbox
\* with its lease, the registration still under way. Elsewhere: S is registered with A from here.
HAttach ==
    /\ hpc = "attach"                                         \* the ask was taken
    /\ IF Fails                                               \* the handle closed first
       THEN /\ regS' = "unreg"                                \* the registration goes
            /\ mail' = mail \cup {Release}                    \* the slot back to A
            /\ hpc' = "clear"                                 \* its name goes after
            /\ UNCHANGED <<home, token>>
       ELSE IF Early                                          \* a ring
       THEN /\ mail' = mail \cup {Install}                    \* the install, with its lease
            /\ hpc' = "next"                                  \* under way
            /\ UNCHANGED <<regS, home, token>>
       ELSE /\ regS' = "registered"                           \* registered with A's poller
            /\ home' = "A"                                    \* naming A
            /\ token' = TokS                                  \* under S's token
            /\ hpc' = "next"
            /\ UNCHANGED mail
    /\ UNCHANGED <<apc, slotGen, slotOf, ask, reqAttach, reqEvict, held, bits, foreign>>

\* A failed attach's name goes, after its release was posted.
HClear ==
    /\ hpc = "clear"                                          \* the release is posted
    /\ home' = "none"                                         \* the name goes
    /\ hpc' = "next"                                          \* on to the eviction
    /\ UNCHANGED <<apc, slotGen, slotOf, ask, reqAttach, reqEvict, regS, token, mail, held, bits,
                   foreign>>

\* The eviction comes after the attach, if it was taken too.
HNext ==
    /\ hpc = "next"                                           \* the attach is done
    /\ hpc' = IF "evict" \in bits THEN "evict" ELSE "end"     \* the eviction, or the end
    /\ UNCHANGED <<apc, slotGen, slotOf, ask, reqAttach, reqEvict, regS, home, token, mail, held,
                   bits, foreign>>

\* `evict_now`: a ring attach under way is abandoned (a release of the token to A); a
\* registration in place ends (a readiness poller's needs no post); none is left alone.
HEvict ==
    /\ hpc = "evict"                                          \* the eviction
    /\ IF regS = "registering"                                \* a ring attach under way
       THEN /\ regS' = "unreg"                                \* abandoned
            /\ mail' = mail \cup {Release}                    \* the slot back to A
            /\ hpc' = "gone"                                  \* the name goes after
       ELSE IF regS = "registered"                            \* registered with A
       THEN /\ regS' = "unreg"                                \* the registration ends
            /\ hpc' = "gone"
            /\ UNCHANGED mail
       ELSE /\ hpc' = "end"                                   \* nothing to end
            /\ UNCHANGED <<regS, mail>>
    /\ UNCHANGED <<apc, slotGen, slotOf, ask, reqAttach, reqEvict, home, token, held, bits,
                   foreign>>

\* `registration_gone`: the token and the shard go last.
HGone ==
    /\ hpc = "gone"                                           \* the posts are in
    /\ home' = "none"                                         \* no shard named
    /\ token' = -1                                            \* and no token
    /\ hpc' = "end"                                           \* done performing
    /\ UNCHANGED <<apc, slotGen, slotOf, ask, reqAttach, reqEvict, regS, mail, held, bits,
                   foreign>>

\* The holder lets go of both sides; a bit raised meanwhile it performs as well, still holding.
HEnd ==
    /\ hpc = "end"                                            \* a perform ended
    /\ IF reqAttach \/ reqEvict                               \* asked for meanwhile
       THEN /\ hpc' = "hold"                                  \* it performs again
            /\ UNCHANGED held
       ELSE /\ held' = FALSE                                  \* else it lets go
            /\ hpc' = "done"                                  \* for good
    /\ UNCHANGED <<apc, slotGen, slotOf, ask, reqAttach, reqEvict, regS, home, token, mail, bits,
                   foreign>>

(***************************************************************************)
(* The whole system.                                                       *)
(***************************************************************************)

Next ==
    \/ ADrain \/ AEvict \/ AWaitAsk \/ AWaitReg \/ ALook \/ AAgain \/ AApply   \* shard A
    \/ HTake \/ HPub \/ HTakeAsk \/ HAttach \/ HClear \/ HNext \/ HEvict \/ HGone \/ HEnd  \* H

\* Every step that must happen does, if it stays possible.
Fairness ==
    /\ WF_vars(ADrain) /\ WF_vars(AEvict) /\ WF_vars(AWaitAsk) /\ WF_vars(AWaitReg)
    /\ WF_vars(ALook) /\ WF_vars(AAgain) /\ WF_vars(AApply)
    /\ WF_vars(HTake) /\ WF_vars(HPub) /\ WF_vars(HTakeAsk) /\ WF_vars(HAttach)
    /\ WF_vars(HClear) /\ WF_vars(HNext) /\ WF_vars(HEvict) /\ WF_vars(HGone) /\ WF_vars(HEnd)

Spec == Init /\ [][Next]_vars /\ Fairness                    \* the behaviours checked

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* A release never removes S2's registration.
NoForeignRelease == ~foreign

\* While A is away, no install waits in its mailbox with the lease that keeps S's descriptor open.
NoLeaseLeft == apc = "away" => ~(\E m \in mail : m.kind = "install")

\* Everything ends: A runs again and the holder let go.
AllEnd == <>(apc = "done" /\ hpc = "done")

(***************************************************************************)
(* Witness: claims an interesting case never happens and must be proved     *)
(* wrong.                                                                  *)
(***************************************************************************)

\* A release reaches A only at its next block_on, after S2 took the slot.
NoLateRelease == ~(apc = "apply2" /\ \E m \in mail : m.kind = "release")
=============================================================================

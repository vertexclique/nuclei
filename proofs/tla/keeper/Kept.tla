-------------------------------- MODULE Kept --------------------------------
(***************************************************************************)
(* A shard's ring after its shard went, and the mail another thread posts  *)
(* to the shard meanwhile.                                                 *)
(*                                                                          *)
(* The Rust it models: the shard's drop, `Shard::go` and `keep_ring`       *)
(* (src/rt/shard/going.rs); `hand_off`, `Kept::keep` and `apply_mailbox`   *)
(* (src/rt/shard/ring/keeper.rs); `Shared::post_io`, `close_io`,           *)
(* `unpark` and `IoOp::gone` (src/rt/shard.rs); the park handshake         *)
(* `ParkState` (src/rt/handshake.rs); and the wake's read of the ring's    *)
(* eventfd, `RingPoller::arm_wake`, `wait` and `reap`                      *)
(* (src/rt/park/ring.rs), with `Notifier::notify` (src/rt/park/linux.rs).  *)
(*                                                                          *)
(* In plain words: a shard goes for good with one operation still in the   *)
(* kernel (a zero-copy send that waits for its peer). It applies its       *)
(* mailbox once and hands its ring to a keeper, which applies the ring's   *)
(* completions and the mailbox until nothing is out, then lets go of the   *)
(* ring and closes the mailbox. Another thread posts one op to the         *)
(* mailbox at any moment: a source's registration ("register", which it    *)
(* claimed first), or a ring source's detach ("detach", whose cancel is    *)
(* what ends the operation out). The poster pushes, looks whether the      *)
(* mailbox closed (then it lets go of what it posted itself), and wakes    *)
(* the shard only if the park handshake says it sleeps; the keeper takes   *)
(* part in that handshake, so a post finds it parked and its wake ends the *)
(* keeper's wait. The wake is a write to the ring's eventfd: a read of the *)
(* eventfd stays armed in the ring while it waits, and that read's 8       *)
(* bytes land in memory of the process. A waker that saw the keeper parked *)
(* may write the eventfd only after the keeper let go of the ring; so may  *)
(* a thread that kept the shard's notifier (`Proactor::wake`). The kernel  *)
(* may keep the ring past its poller (a receive group's copy of the ring's *)
(* descriptor holds it), so that read may still complete then.             *)
(*                                                                          *)
(* What must never happen: the read writing memory that was freed          *)
(* (NoWriteAfterFree); a registration left claimed with no shard to make   *)
(* it (NoStuckRegistration); an op left in the mailbox once everyone is    *)
(* done (NoStranded); and everything must end (Ends): the keeper wakes for *)
(* a detach posted while it waits, and lets go of the ring.                *)
(*                                                                          *)
(* What is abstract: the memory orderings (the handshake's loom models and *)
(* proofs/tla/park check them), the ring's other operations (LentZc.tla),  *)
(* and the eventfd's count, which is one bit here: a read takes it all.    *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Op,        \* what the other thread posts: "register" or "detach"
    Stray,     \* whether a thread that kept the shard's notifier writes its eventfd once, any time
    Mutation   \* "none", or the name of the one rule this run takes away

VARIABLES
    ppc,       \* the poster's place: "start" "push" "look" "wake" "notify" "done"
    reg,       \* the registration the poster claimed: "unreg" or "registering"
    mail,      \* ops waiting in the shard's mailbox: 0 or 1
    handled,   \* what became of the posted op: "no" "applied" "gone" "dropped"
    closed,    \* nothing applies the mailbox from now on (`io_closed`)
    parked,    \* the park handshake's flag: the shard, or its keeper, says it sleeps
    spc,       \* the going shard's place: "apply" "hand" "done"
    kpc,       \* the keeper's place: "none" "apply" "park" "look" "wait" "sleep" "drop" "close" "done"
    out,       \* the operation the kernel still holds: 1, or 0 once it completed
    cqe,       \* its completion waits in the ring to be reaped
    efd,       \* the eventfd was written and its read has not taken that yet
    armed,     \* the wake's read of the eventfd is in the kernel
    woken,     \* the wake's read completed and waits in the ring to be reaped
    freed,     \* the 8 bytes the wake's read lands in were freed
    scribbled, \* (record) the kernel wrote those bytes after they were freed
    lateRead,  \* (record) the wake's read completed after the ring's poller went
    postWake,  \* (record) the keeper's wait ended by a wake, with the operation still out
    xpc        \* the stray waker's place: "idle" "done"

vars == <<ppc, reg, mail, handled, closed, parked, spc, kpc, out, cqe, efd, armed, woken,
          freed, scribbled, lateRead, postWake, xpc>>   \* every variable

(***************************************************************************)
(* What happens to the posted op.                                          *)
(***************************************************************************)

\* The ring's operation is still the ring's: in the kernel, or its completion not reaped yet.
InFlight == out = 1 \/ cqe

\* The poller of the ring is gone: the keeper dropped it, or the shard did with nothing out.
PollerGone == kpc \in {"close", "done"} \/ (kpc = "none" /\ spc = "done")

\* `who` applies the op in the mailbox. A registration applied by the going shard is refused for
\* its stop and goes back to none (`apply_io_ops`, `is_stopped_shard`); one applied by the keeper
\* goes back to none through `IoOp::gone` (`registration_orphaned`). A detach is applied by either.
\* MUTATION keeper_drops: the keeper drops every op but a ring's detach or cancel (the code before).
Apply(who) ==
    LET drops == who = "keeper" /\ Op = "register" /\ Mutation = "keeper_drops"  \* the planted bug
    IN /\ mail' = 0                                          \* the op leaves the mailbox
       /\ handled' = IF drops THEN "dropped" ELSE "applied"  \* applied, or lost
       /\ reg' = IF Op = "register" /\ ~drops THEN "unreg" ELSE reg  \* the claim ends, unless lost

\* Whoever drains a mailbox nothing applies lets go of what it holds with `IoOp::gone`: a
\* registration goes back to none, a detach's lease goes (its object may close: the ring is gone).
Drain ==
    IF mail = 1                                              \* an op waits
    THEN /\ mail' = 0                                        \* it leaves the mailbox
         /\ handled' = "gone"                                \* let go of
         /\ reg' = IF Op = "register" THEN "unreg" ELSE reg  \* a claim ends
    ELSE UNCHANGED <<mail, handled, reg>>                    \* nothing to let go of

Init ==
    /\ ppc = "start"            \* the poster has not begun
    /\ reg = "unreg"            \* nothing is claimed
    /\ mail = 0                 \* the mailbox is empty
    /\ handled = "no"           \* nothing was posted
    /\ closed = FALSE           \* the mailbox is open
    /\ parked = FALSE           \* nobody says it sleeps
    /\ spc = "apply"            \* the shard is going: its one look at the mailbox comes first
    /\ kpc = "none"             \* no keeper yet
    /\ out = 1                  \* one operation is in the kernel
    /\ cqe = FALSE              \* with no completion yet
    /\ efd = FALSE              \* the eventfd was not written
    /\ armed = TRUE             \* the shard's last wait left the wake's read armed
    /\ woken = FALSE            \* that read has not completed
    /\ freed = FALSE            \* its bytes are in place
    /\ scribbled = FALSE        \* nothing wrote freed memory
    /\ lateRead = FALSE         \* no read completed after the poller went
    /\ postWake = FALSE         \* no wake ended the keeper's wait yet
    /\ xpc = "idle"             \* the stray waker has not written

(***************************************************************************)
(* The poster: `Shared::post_io` on another thread.                        *)
(***************************************************************************)

\* A registration is claimed first (`Source::register`'s compare-and-swap); a detach needs none.
PStart ==
    /\ ppc = "start"                                         \* the poster begins
    /\ reg' = IF Op = "register" THEN "registering" ELSE reg \* it claims the registration
    /\ ppc' = "push"                                         \* and posts next
    /\ UNCHANGED <<mail, handled, closed, parked, spc, kpc, out, cqe, efd, armed, woken, freed,
                   scribbled, lateRead, postWake, xpc>>

\* The op goes into the mailbox.
PPush ==
    /\ ppc = "push"                                          \* about to push
    /\ mail' = 1                                             \* the op waits in the mailbox
    /\ ppc' = "look"                                         \* then the poster looks at the flag
    /\ UNCHANGED <<reg, handled, closed, parked, spc, kpc, out, cqe, efd, armed, woken, freed,
                   scribbled, lateRead, postWake, xpc>>

\* The poster looks whether the mailbox closed: then nothing applies it, and the poster drains it.
\* MUTATION post_no_look: the poster never looks, so an op posted after the close stays for good.
PLook ==
    /\ ppc = "look"                                          \* pushed, about to look
    /\ IF closed /\ Mutation # "post_no_look"                \* closed: nobody applies it
       THEN /\ Drain                                         \* so the poster lets go of it
            /\ ppc' = "done"                                 \* and is done
       ELSE /\ ppc' = "wake"                                 \* open: it wakes the shard next
            /\ UNCHANGED <<mail, handled, reg>>
    /\ UNCHANGED <<closed, parked, spc, kpc, out, cqe, efd, armed, woken, freed, scribbled,
                   lateRead, postWake, xpc>>

\* `ParkState::wake`: only a waker that finds the flag up, and swaps it down, notifies.
PWake ==
    /\ ppc = "wake"                                          \* about to wake
    /\ IF parked                                             \* someone says it sleeps
       THEN /\ parked' = FALSE                               \* the swap takes the flag down
            /\ ppc' = "notify"                               \* and this waker will notify
       ELSE /\ ppc' = "done"                                 \* nobody sleeps: no notification
            /\ UNCHANGED parked
    /\ UNCHANGED <<reg, mail, handled, closed, spc, kpc, out, cqe, efd, armed, woken, freed,
                   scribbled, lateRead, postWake, xpc>>

\* The notification: a write to the eventfd. It may come any time after the swap, even after
\* the keeper woke for another reason and let go of the ring.
PNotify ==
    /\ ppc = "notify"                                        \* swapped, about to write
    /\ efd' = TRUE                                           \* the eventfd is written
    /\ ppc' = "done"                                         \* the poster is done
    /\ UNCHANGED <<reg, mail, handled, closed, parked, spc, kpc, out, cqe, armed, woken, freed,
                   scribbled, lateRead, postWake, xpc>>

(***************************************************************************)
(* The going shard: `Shard::go` and `keep_ring`.                           *)
(***************************************************************************)

\* The shard's one look at its mailbox as it goes.
SApply ==
    /\ spc = "apply"                                         \* the shard is going
    /\ IF mail = 1 THEN Apply("shard") ELSE UNCHANGED <<mail, handled, reg>>  \* applies what waits
    /\ spc' = "hand"                                         \* then decides about its ring
    /\ UNCHANGED <<ppc, closed, parked, kpc, out, cqe, efd, armed, woken, freed, scribbled,
                   lateRead, postWake, xpc>>

\* With an operation out the ring goes to a keeper, which applies the mailbox from now on; with
\* none the shard closes the mailbox, and its poller goes with the shard.
\* MUTATION buf_with_poller: the wake's read lands in memory the poller frees (the code before).
SHand ==
    /\ spc = "hand"                                          \* the shard looked at its mailbox
    /\ spc' = "done"                                         \* and is gone after this step
    /\ IF InFlight                                           \* an operation is out
       THEN /\ kpc' = "apply"                                \* a keeper takes the ring
            /\ UNCHANGED <<closed, mail, handled, reg, freed>>
       ELSE /\ closed' = TRUE                                \* the mailbox closes
            /\ Drain                                         \* what it holds is let go of
            /\ freed' = (Mutation = "buf_with_poller")       \* the poller goes with its memory
            /\ UNCHANGED kpc
    /\ UNCHANGED <<ppc, parked, out, cqe, efd, armed, woken, scribbled, lateRead, postWake, xpc>>

(***************************************************************************)
(* The keeper: `Kept::keep`.                                               *)
(***************************************************************************)

\* The loop's head: with nothing out it lets go of the ring; else it applies the mailbox.
KApply ==
    /\ kpc = "apply"                                         \* the keeper's loop
    /\ IF InFlight                                           \* the operation is still the ring's
       THEN /\ IF mail = 1 THEN Apply("keeper") ELSE UNCHANGED <<mail, handled, reg>>
            /\ kpc' = "park"                                 \* then it gets ready to wait
       ELSE /\ kpc' = "drop"                                 \* nothing out: the ring goes
            /\ UNCHANGED <<mail, handled, reg>>
    /\ UNCHANGED <<ppc, closed, parked, spc, out, cqe, efd, armed, woken, freed, scribbled,
                   lateRead, postWake, xpc>>

\* The keeper says it sleeps (`ParkState::begin_park`), so a post from now on notifies it.
\* MUTATION no_flag: the keeper never says it sleeps (the code before), so no post wakes it.
KPark ==
    /\ kpc = "park"                                          \* about to wait
    /\ IF Mutation = "no_flag"                               \* the planted bug
       THEN /\ kpc' = "wait"                                 \* straight to the wait
            /\ UNCHANGED parked
       ELSE /\ parked' = TRUE                                \* the flag goes up first
            /\ kpc' = "look"                                 \* then it looks once more
    /\ UNCHANGED <<ppc, reg, mail, handled, closed, spc, out, cqe, efd, armed, woken, freed,
                   scribbled, lateRead, postWake, xpc>>

\* The look after the flag: mail that came first is applied, and the keeper does not sleep.
\* MUTATION no_look: the keeper sleeps without that look, and misses a post that saw no flag.
KLook ==
    /\ kpc = "look"                                          \* the flag is up
    /\ IF mail = 1 /\ Mutation # "no_look"                   \* mail came before it
       THEN /\ parked' = FALSE                               \* it backs out of the park
            /\ kpc' = "apply"                                \* and applies the mail
       ELSE /\ kpc' = "wait"                                 \* nothing: it waits
            /\ UNCHANGED parked
    /\ UNCHANGED <<ppc, reg, mail, handled, closed, spc, out, cqe, efd, armed, woken, freed,
                   scribbled, lateRead, postWake, xpc>>

\* The wait arms the wake's read first, if it is not armed (`arm_wake`).
KWait ==
    /\ kpc = "wait"                                          \* about to enter the kernel
    /\ armed' = TRUE                                         \* the wake's read is in
    /\ kpc' = "sleep"                                        \* and the keeper sleeps
    /\ UNCHANGED <<ppc, reg, mail, handled, closed, parked, spc, out, cqe, efd, woken, freed,
                   scribbled, lateRead, postWake, xpc>>

\* The wait ends with a completion or a wake, which it reaps; the flag goes down
\* (`ParkState::end_park`), and the loop goes round.
KSleep ==
    /\ kpc = "sleep"                                         \* the keeper sleeps
    /\ cqe \/ woken                                          \* until something completes
    /\ postWake' = (postWake \/ (woken /\ out = 1))          \* (record) a wake, the op still out
    /\ cqe' = FALSE                                          \* the operation's completion is reaped
    /\ woken' = FALSE                                        \* and the wake's
    /\ parked' = FALSE                                       \* the keeper is awake
    /\ kpc' = "apply"                                        \* round the loop
    /\ UNCHANGED <<ppc, reg, mail, handled, closed, spc, out, efd, armed, freed, scribbled,
                   lateRead, xpc>>

\* The ring goes (`drop(ring)`): its poller with it. The kernel may keep the ring, and with it the
\* wake's read, still armed.
KDrop ==
    /\ kpc = "drop"                                          \* nothing is out
    /\ freed' = (Mutation = "buf_with_poller")               \* the planted bug frees the bytes
    /\ kpc' = "close"                                        \* the mailbox closes next
    /\ UNCHANGED <<ppc, reg, mail, handled, closed, parked, spc, out, cqe, efd, armed, woken,
                   scribbled, lateRead, postWake, xpc>>

\* `close_io`: nothing applies the mailbox from now on, and what it holds is let go of.
KClose ==
    /\ kpc = "close"                                         \* the ring went
    /\ closed' = TRUE                                        \* the flag goes up
    /\ Drain                                                 \* and the keeper drains the mailbox
    /\ kpc' = "done"                                         \* it is done
    /\ UNCHANGED <<ppc, parked, spc, out, cqe, efd, armed, woken, freed, scribbled, lateRead,
                   postWake, xpc>>

(***************************************************************************)
(* The kernel, and a stray waker.                                          *)
(***************************************************************************)

\* The operation completes. A detach's cancel is what ends it (a peer that never reads), so with a
\* detach posted it completes only once that detach was applied or let go of.
KernelEnd ==
    /\ out = 1                                               \* still in the kernel
    /\ Op = "register" \/ handled \in {"applied", "gone"}    \* nothing holds it back
    /\ out' = 0                                              \* it completed
    /\ cqe' = TRUE                                           \* and its completion waits
    /\ UNCHANGED <<ppc, reg, mail, handled, closed, parked, spc, kpc, efd, armed, woken, freed,
                   scribbled, lateRead, postWake, xpc>>

\* The armed read takes the eventfd's count: it writes its 8 bytes, and its completion waits.
WakeRead ==
    /\ armed                                                 \* the read is in the kernel
    /\ efd                                                   \* and the eventfd was written
    /\ armed' = FALSE                                        \* the read is done
    /\ efd' = FALSE                                          \* it took the count
    /\ woken' = TRUE                                         \* its completion waits
    /\ scribbled' = (scribbled \/ freed)                     \* (record) its bytes were freed
    /\ lateRead' = (lateRead \/ PollerGone)                  \* (record) the poller had gone
    /\ UNCHANGED <<ppc, reg, mail, handled, closed, parked, spc, kpc, out, cqe, freed, postWake,
                   xpc>>

\* A thread that kept the shard's notifier writes the eventfd (`Proactor::wake`).
StrayWake ==
    /\ Stray                                                 \* this run has such a thread
    /\ xpc = "idle"                                          \* it has not written
    /\ efd' = TRUE                                           \* it writes the eventfd
    /\ xpc' = "done"                                         \* once
    /\ UNCHANGED <<ppc, reg, mail, handled, closed, parked, spc, kpc, out, cqe, armed, woken,
                   freed, scribbled, lateRead, postWake>>

(***************************************************************************)
(* The whole system.                                                       *)
(***************************************************************************)

\* Everyone is done: the poster, the shard, and the keeper if there was one.
AllDone == ppc = "done" /\ spc = "done" /\ kpc \in {"none", "done"}

Next ==
    \/ PStart \/ PPush \/ PLook \/ PWake \/ PNotify          \* the poster
    \/ SApply \/ SHand                                       \* the going shard
    \/ KApply \/ KPark \/ KLook \/ KWait \/ KSleep \/ KDrop \/ KClose  \* the keeper
    \/ KernelEnd \/ WakeRead                                 \* the kernel
    \/ StrayWake                                             \* the stray waker

\* Every step that must happen does, if it stays possible. The stray waker may never write.
Fairness ==
    /\ WF_vars(PStart) /\ WF_vars(PPush) /\ WF_vars(PLook) /\ WF_vars(PWake) /\ WF_vars(PNotify)
    /\ WF_vars(SApply) /\ WF_vars(SHand)
    /\ WF_vars(KApply) /\ WF_vars(KPark) /\ WF_vars(KLook) /\ WF_vars(KWait) /\ WF_vars(KSleep)
    /\ WF_vars(KDrop) /\ WF_vars(KClose)
    /\ WF_vars(KernelEnd) /\ WF_vars(WakeRead)

Spec == Init /\ [][Next]_vars /\ Fairness                   \* the behaviours checked

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* The wake's read never writes memory that was freed.
NoWriteAfterFree == ~scribbled

\* Once everyone is done, no registration is left claimed with nobody to make it.
NoStuckRegistration == AllDone => reg # "registering"

\* Once everyone is done, nothing waits in the mailbox.
NoStranded == AllDone => mail = 0

\* Everything ends: the keeper applies a detach posted while it waits, the operation completes,
\* and the keeper lets go of the ring.
Ends == <>AllDone

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A post wakes the keeper while the operation is still out.
NoPostWake == ~postWake
\* The wake's read completes after the ring's poller went.
NoLateRead == ~lateRead
=============================================================================

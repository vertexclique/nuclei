------------------------------- MODULE Route -------------------------------
(***************************************************************************)
(* A read of a ring source from a thread that runs no shard, while the     *)
(* source's registration with the ring finishes (src/io/source.rs,        *)
(* `Source::with_io` with its route read `Source::via` and its look after *)
(* the step `Source::submit_if_registered`, `ensure_registered`,           *)
(* `ops_registered`; the side's `submit_wanted` in                         *)
(* src/io/source/read/ops.rs; `RingIo::issuer` in                         *)
(* src/io/source/ring.rs), at the grain of the read side's holds.          *)
(*                                                                          *)
(* Who acts. The reader, a task on a thread that runs no shard. The ring's *)
(* thread, which finishes the registration (the install, a request done   *)
(* with both sides held, then `ops_registered`) and submits what a kick    *)
(* asks for.                                                               *)
(*                                                                          *)
(* What can happen. `with_io` reads the route before the side is taken:   *)
(* NotYet while the registration is under way, Elsewhere once it is in    *)
(* place (the ring's thread submits, after a kick). The reader takes the  *)
(* read side; its step finds nothing to read, leaves a waiter and asks    *)
(* `submit_wanted` with that route: NotYet issues nothing, Elsewhere posts *)
(* a kick. The registration, when the ring's thread finds the side free,  *)
(* is done there with `ops_registered`, which submits for a waiter it     *)
(* finds; when the side is held it is left as a request, and the holder   *)
(* does it as it lets go, then asks with a fresh route. A kick finding the *)
(* side held leaves a submit for its holder. A side held makes a poll     *)
(* leave its waker and sleep until the holder wakes it. After its step,   *)
(* the reader looks once more (the fix, commit 4bc2518): if its route was *)
(* NotYet and the source is registered now, it asks the side to submit,   *)
(* through the same path (`defer` of SUBMIT: done now if the side is free, *)
(* else by its holder).                                                    *)
(*                                                                          *)
(* What must never happen. A step leaves a waiter and no operation is ever *)
(* submitted for it (the read waits for good). An operation is submitted  *)
(* twice for one waiter.                                                   *)
(*                                                                          *)
(* What is abstract. The side's word is one holder and its left flags     *)
(* (`side` checks it); the request needs both sides, of which the read    *)
(* side is the one that matters (Requests.tla checks the pair); an        *)
(* operation submitted is the read served (its completion wakes the task). *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC                                             \* numbers, sequences (the mailbox), TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Registered,                                                              \* TRUE: the registration is in place before the read
    Mutation                                                                 \* "none", or the name of the one rule this run takes away

(* --algorithm Route                                                           \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    reg = IF Registered THEN "registered" ELSE "registering",   \* the registration
    mailbox = IF Registered THEN <<>> ELSE <<"install">>,       \* the ring thread's posted work, in order
    holder = "none",                                            \* who holds the read side: "none", "reader", "ring"
    reqFlag = FALSE,                                            \* REQ left: the registration's request, for the holder
    submitFlag = FALSE,                                         \* SUBMIT left: a submit, for the holder
    waitFlag = FALSE,                                           \* WAITING left: the reader sleeps until the holder wakes it
    woken = FALSE,                                              \* the reader was woken and has not polled again
    waiter = FALSE,                                             \* the reader's step left a waiter
    submits = 0,                                                \* operations submitted for the waiter
    lateResubmit = FALSE;                                       \* (record) the look after the step asked for a submit

define                                                                       \* names the code and the properties use:
    \* What the route says once the registration is in place, for the reader: the ring's thread submits.
    Fresh == IF reg = "registered" THEN "elsewhere" ELSE "notyet"            \* the reader's route read now
    \* A kick is needed: a waiter with no operation out, and a route that reaches the ring.
    Wants == waiter /\ submits = 0                                           \* the side wants an operation
end define;                                                                  \* end of the names

\* The reader (`Source::with_io`, then `Side::poll` and the step).
fair process reader = "reader"                                               \* weakly fair, like every process
variables route = "notyet", wasNotYet = FALSE;                               \* the route it read; that it read NotYet
begin                                                                        \* the reader's steps:
R_route:                                                                     \* the route, read before the side is taken
    route := Fresh;                                                          \* NotYet while under way, else Elsewhere
    wasNotYet := (reg # "registered");                                       \* it read NotYet
R_take:                                                                      \* the read side
    if holder = "none" then holder := "reader";                              \* free: the reader holds it
    else                                                                     \* held: the poll leaves its waker,
        waitFlag := TRUE;                                                    \* raises WAITING,
        goto R_park;                                                         \* and sleeps
    end if;                                                                  \* holding
R_step:                                                                      \* the step: nothing to read
    waiter := TRUE;                                                          \* a waiter is left,
    if route = "elsewhere" /\ submits = 0 then                               \* and `submit_wanted` with its route:
        mailbox := Append(mailbox, "kick");                                  \* Elsewhere kicks the ring's thread
    end if;                                                                  \* NotYet issues nothing
R_rel:                                                                       \* the reader lets go, doing what was left
    if reqFlag then reg := "registered"; end if;                             \* the request: done here (`perform`, `attach_now`)
    \* `ops_registered` after the request, or a submit a kick left (only ever once registered), asks with a fresh
    \* route, which is Elsewhere for this thread: a kick.
    if (reqFlag \/ submitFlag) /\ Wants then                                \* something asks, and the side wants an operation:
        mailbox := Append(mailbox, "kick");                                  \* the ring's thread is kicked
    end if;                                                                  \* the asks are done
    reqFlag := FALSE;                                                        \* both flags
    submitFlag := FALSE;                                                     \* taken,
    holder := "none";                                                        \* and the side is free
R_post:                                                                      \* the look after the step (the fix)
    \* MUTATION no_resubmit: the reader does not look again after its step.
    if wasNotYet /\ reg = "registered" /\ Mutation # "no_resubmit" then      \* its route was NotYet, the source registered now:
        lateResubmit := TRUE;                                                \* (record)
        if holder = "none" then                                              \* `defer` of SUBMIT, the side free:
            if Wants then mailbox := Append(mailbox, "kick"); end if;        \* done now, with a fresh route
        else                                                                 \* held:
            submitFlag := TRUE;                                              \* left for its holder
        end if;                                                              \* either way the side is asked
    end if;                                                                  \* else nothing
R_wait:                                                                      \* the read waits for its operation
    await submits > 0;                                                       \* submitted: its completion wakes the task
    goto Done;                                                               \* the read is served
R_park:                                                                      \* asleep until the holder wakes it
    await woken;                                                             \* woken
    woken := FALSE;                                                          \* the wake is taken
    goto R_route;                                                            \* and the task polls again, from the route
end process;                                                                 \* end of the reader

\* The ring's thread: its posted work, in order.
fair process ring = "ring"                                                   \* weakly fair, like every process
variables msg = "none";                                                      \* the work it took
begin                                                                        \* the ring thread's steps:
G_loop:                                                                      \* the next posted work
    await mailbox # <<>> \/ pc["reader"] = "Done";                           \* work, or the read is served
    if mailbox = <<>> then goto Done;                                        \* nothing more will come
    else                                                                     \* the oldest:
        msg := Head(mailbox);                                                \* taken
        mailbox := Tail(mailbox);                                            \* off the mailbox
    end if;                                                                  \* now it is done
G_take:                                                                      \* the side, for that work
    if holder = "none" then                                                  \* free: the ring's thread holds it
        holder := "ring";                                                    \* held,
        if msg = "install" then reg := "registered"; end if;                 \* the registration done with the sides held,
        if Wants then submits := submits + 1; end if;                        \* and the side's want submitted here (Here)
    elsif msg = "install" then                                               \* held, the registration:
        reqFlag := TRUE;                                                     \* left as a request for the holder
        goto G_loop;                                                         \* the ring's thread goes on
    else                                                                     \* held, a kick:
        submitFlag := TRUE;                                                  \* left as a submit for the holder
        goto G_loop;                                                         \* the ring's thread goes on
    end if;                                                                  \* holding
G_rel:                                                                       \* the ring's thread lets go
    if submitFlag then                                                       \* a submit left meanwhile:
        submitFlag := FALSE;                                                 \* taken,
        if Wants then submits := submits + 1; end if;                        \* and done here
    end if;                                                                  \* the submit is done
    if waitFlag then                                                         \* a poll waits:
        waitFlag := FALSE;                                                   \* WAITING taken,
        woken := TRUE;                                                       \* and it is woken
    end if;                                                                  \* the waits are done
    holder := "none";                                                        \* the side is free
    goto G_loop;                                                             \* the next work
end process;                                                                 \* end of the ring's thread
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* A step that left a waiter has its operation submitted in the end: no read waits for good.
Served == waiter ~> (submits > 0)                                            \* every waiter is served

\* An operation is submitted once for the waiter.
OneSubmit == submits <= 1                                                    \* never twice

\* `Termination` (every process ends, so the read was served) is the one the translator writes for the fair processes.

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* The reader's look after its step finds the registration done since its route and asks for a submit.
NoLateResubmit == ~lateResubmit                                              \* never the late path
=============================================================================

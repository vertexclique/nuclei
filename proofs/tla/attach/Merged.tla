------------------------------- MODULE Merged -------------------------------
(***************************************************************************)
(* Requests that wait for the sides together, performed in one go, and a  *)
(* retire's attach refused for it (src/io/source/requests.rs: `perform`,   *)
(* `attach_now`, `attach`, `retire`, `is_refused_for_now`;                 *)
(* src/io/source.rs: `Source::register`, `revive`; src/rt/shard.rs:        *)
(* `Shard::register_source`, `apply_io_ops`).                              *)
(*                                                                          *)
(* Who acts. A holder of the read side (a pool's buffer coming back to the *)
(* source holds it for a moment): whoever holds a side when a request      *)
(* comes performs it as it lets go, with every request pending. A task     *)
(* whose poll registers the source: it moves the registration to          *)
(* "registering" and asks the attach, at once on its own shard, or, on a  *)
(* thread that runs none, by posting the registration to a shard, which   *)
(* asks it later. Then its future is dropped. The handle's owner, who      *)
(* calls `get_mut` once no future borrows the handle: it asks the retire   *)
(* and waits until it was performed; the descriptor is out of the source   *)
(* meanwhile. Then it reads: the read brings the same descriptor back      *)
(* (`revive`), and what the sides kept stays for it.                       *)
(*                                                                          *)
(* What can happen. The holder may let go with the retire and the attach   *)
(* both pending: `perform` takes them in its order, the retire first, so   *)
(* the attach finds no descriptor. Or the shard's posted attach comes once *)
(* the retire was performed, and the shard performs it itself.             *)
(*                                                                          *)
(* What must never happen. The owner's first read after `get_mut`, on a   *)
(* descriptor that is valid and with nothing failed, answers an error: an  *)
(* attach refused because the descriptor was out only for the retire must  *)
(* fail nothing, as one refused for a shard that stopped fails nothing; it *)
(* leaves the source unregistered, and the read registers it again.       *)
(*                                                                          *)
(* What is abstract. One source, one attach, one retire; a side is one    *)
(* flag; the wait for the retire's leases is the wait for the retire;     *)
(* a perform is one step, as the sides make it.                            *)
(***************************************************************************)
EXTENDS Integers, TLC                                                        \* numbers and TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Posted,    \* TRUE: the registering thread runs no shard, so a shard asks the attach later; FALSE: at once
    Mutation   \* "none", or the name of the one rule this run takes away

(* --algorithm Merged                                                         \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    readHeld = TRUE,       \* a holder has the read side (it lets go later, performing what waits)
    pending = {},          \* requests asked and not performed: "attach", "retire", "fail"
    fdIn = TRUE,           \* the descriptor is in the source (the retire takes it out, the read brings it back)
    reg = "unreg",         \* the registration: "unreg", "registering", "registered"
    failed = FALSE,        \* the sides keep a failure, which the next call on them answers
    retired = FALSE,       \* the retire was performed
    heard = FALSE,         \* the last perform's requester heard its attach refused for a missing descriptor
    readAnswered = FALSE,  \* the owner's read returned
    spurious = FALSE,      \* (record) the owner's read answered an error on a valid descriptor
    merged = FALSE,        \* (record) one perform took the retire and the attach together
    lateRefusal = FALSE;   \* (record) an attach was refused because the retire took the descriptor out


\* `perform`, with both sides held: every request of `asked`, in the code's order (the retire, the
\* failure, the attach), in one step. `here` is TRUE when the requester whose ask brought it here
\* performs: an attach's answer goes back to its requester (`heard`); one performed by anyone else
\* answers nobody, and in the code fails the sides if the attach failed.
macro Perform(asked, here) begin                                             \* one step under both sides
    with r = "retire" \in asked,                                             \* the retire is asked
         f = "fail" \in asked,                                               \* a registration's failure is left
         a = "attach" \in asked,                                             \* the attach is asked
         fd = fdIn /\ ~r do                                                  \* the descriptor, once the retire went
        fdIn := fd;                                                          \* the retire takes it out of the source
        reg := IF a THEN (IF fd THEN "registered" ELSE "unreg")              \* the attach: registered, or refused and none
               ELSE IF r /\ reg = "registered" THEN "unreg" ELSE reg;        \* the retire: a registration goes
        retired := retired \/ r;                                             \* the retire's requester may go on
        \* Both sides keep a failure that was left; an attach refused for a missing descriptor
        \* fails nothing, as one refused for a shard that stopped.
        \* MUTATION holder_fails: an attach another performer refused for a missing descriptor
        \* fails the sides, as a failed registration does.
        failed := failed \/ f \/ (a /\ ~here /\ ~fd /\ Mutation = "holder_fails");
        heard := a /\ here /\ ~fd;                                            \* its requester hears the refusal
        merged := merged \/ (a /\ r);                                         \* (recorded: taken with the retire)
        lateRefusal := lateRefusal \/ (a /\ ~fd);                             \* (recorded: refused for the retire)
        pending := {};                                                       \* nothing waits now
    end with;                                                                \* (performed)
end macro;                                                                   \* end of the perform

\* A request `req`: left to the holder while it has the read side, else performed by its requester
\* at once, with whatever else waits.
macro Ask(req) begin                                                         \* one fetch_or and a try at the sides
    if readHeld then                                                         \* a side is held:
        pending := pending \cup {req};                                       \* its holder performs it
    else                                                                     \* both sides free:
        Perform(pending \cup {req}, TRUE);                                   \* the requester performs
    end if;                                                                  \* (asked)
end macro;                                                                   \* end of the ask

\* The holder of the read side: it lets go at some point, performing what waits.
fair process holder = "holder"                                               \* weakly fair: it lets go
begin                                                                        \* its one step:
H_release:                                                                   \* the release
    readHeld := FALSE;                                                       \* the read side is free
    if pending # {} then Perform(pending, FALSE); end if;                    \* what waited is performed here
end process;                                                                 \* end of the holder

\* The task whose poll registers the source, then goes away.
fair process registrar = "registrar"                                         \* weakly fair
begin                                                                        \* its steps:
G_cas:                                                                       \* `register`: unreg to registering
    reg := "registering";                                                    \* the registration is under way
G_attach:                                                                    \* on its own shard it asks the attach now
    if ~Posted then Ask("attach"); end if;                                   \* (posted: a shard asks it later)
G_drop:                                                                      \* its future is dropped
    skip;                                                                    \* no task borrows the handle now
end process;                                                                 \* end of the registrar

\* The shard a posted registration went to: it asks the attach when it applies its mail, maybe
\* after the retire. Performing it itself, it hears the answer: a refusal for a missing descriptor
\* is one to look again after, as a stopped shard's, not a failure (`is_refused_for_now`).
fair process shard = "shard"                                                 \* weakly fair: it applies its mail
begin                                                                        \* its steps:
S_apply:                                                                     \* the posted registration
    await ~Posted \/ pc["registrar"] # "G_cas";                              \* posted once the registration is under way
    if Posted then Ask("attach"); end if;                                    \* asked; performed here, or by the holder
S_answer:                                                                    \* what it heard, if it performed the attach
    \* A refusal for a missing descriptor: the registration is back to none and the tasks look
    \* again (`look_again`), no failure.
    \* MUTATION requester_fails: it is a registration's failure, left for the sides
    \* (`registration_failed`), which the next performer applies.
    if heard /\ Mutation = "requester_fails" then Ask("fail"); end if;      \* (the mutation's failure)
end process;                                                                 \* end of the shard

\* The handle's owner: `get_mut` once no future borrows the handle, then a read.
fair process owner = "owner"                                                 \* weakly fair
begin                                                                        \* its steps:
O_retire:                                                                    \* `retire`: asked once the future is gone
    await pc["registrar"] = "Done";                                          \* no future borrows the handle
    Ask("retire");                                                           \* performed by it or the holder (no answer)
O_wait:                                                                      \* it waits until the retire was performed
    await retired;                                                           \* the object may change hands now
O_revive:                                                                    \* the read: the same descriptor back
    fdIn := TRUE;                                                            \* what the sides kept stays
O_read:                                                                      \* the read answers what the side kept,
    if failed then                                                           \* a kept failure:
        spurious := TRUE;                                                    \* an error on a valid descriptor
        readAnswered := TRUE;                                                \* (returned)
        goto Done;                                                           \* the read is over
    elsif reg = "unreg" then                                                 \* no registration:
        reg := "registered";                                                 \* it registers on its own shard
    end if;                                                                  \* (one under way: it waits for it)
O_wait_reg:                                                                  \* the read waits for its registration
    await reg = "registered";                                                \* a posted one may come later
    readAnswered := TRUE;                                                    \* the read returns its bytes
end process;                                                                 \* end of the owner
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* The owner's read after `get_mut`, on a valid descriptor with nothing failed, never answers an error.
NoSpuriousError == ~spurious                                                 \* no error from a merged or late attach

\* `Termination` (every process ends, the owner's read among them) is the one the translator writes
\* for the fair processes.

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* One perform takes the retire and the attach together.
NoMerged == ~merged                                                          \* never together
\* An attach is refused because the retire took the descriptor out.
NoLateRefusal == ~lateRefusal                                                \* never refused so
=============================================================================

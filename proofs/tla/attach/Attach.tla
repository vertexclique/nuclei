------------------------------- MODULE Attach -------------------------------
(***************************************************************************)
(* An attach of a source to a ring performed off the ring's thread, and a  *)
(* close or an evict meanwhile (src/io/source/requests.rs, `perform`,      *)
(* `ring_installed`, `take_installs`; src/io/source/ring.rs,                *)
(* `attach_ring`, `ring_attached`), at the grain of the requests performed *)
(* with both sides held and of the ring thread's steps.                    *)
(*                                                                          *)
(* In plain words: a thread that is not the ring's performs the attach: it *)
(* marks the registration under way and posts the ring a copy of the       *)
(* descriptor. The ring's thread looks whether the registration still     *)
(* wants it, puts the file into its table, and hands the slot to the       *)
(* source as a request. Whoever performs that request, with both sides     *)
(* held, keeps the slot if the attach is still under way and the handle    *)
(* open, else posts the ring to let it go. A close or an evict, performed  *)
(* the same way, undoes an attach under way. The file must never stay in   *)
(* the table of a source that closed, and a source the shard evicted must  *)
(* never end registered there.                                            *)
(*                                                                          *)
(* What is modelled step by step: the attach and every request (each one  *)
(* atomic step, as the sides make them); the ring's look, its install and *)
(* its hand-over as separate steps; the ring's mailbox, in order. What is *)
(* abstract: one source, one shard, one attach; the token is a flag.     *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    Close,     \* TRUE: the handle closes at some point
    Evict,     \* TRUE: the shard evicts the source at some point
    Mutation   \* "none", or the name of the one rule this run takes away

Requesters == {"attacher", "closer", "evicter"}

(* --algorithm Attach
variables
    reg = "unreg",      \* the registration: "unreg", "registering", "registered"
    ops = FALSE,        \* registered with a ring (or under way to)
    tok = FALSE,        \* the token of the attach is the registration's
    fdOpen = TRUE,      \* the handle's descriptor is open
    table = FALSE,      \* the ring's table holds the file
    mailbox = <<>>,     \* the ring's posted work, in order
    stopped = FALSE,    \* the shard is going away (set before it evicts its sources)
    reqs = {},          \* requests raised and not performed
    installed = FALSE,  \* an install handed to the source and not taken
    closed = FALSE,     \* (record) the close was performed
    evicted = FALSE,    \* (record) the evict was performed
    staleDrop = FALSE,  \* (record) the ring found the attach undone and put nothing in
    rejected = FALSE;   \* (record) an install the source no longer wanted went back

define
    RequestersDone == \A p \in Requesters : pc[p] = "Done"
    Quiet == RequestersDone /\ mailbox = <<>> /\ reqs = {}
end define;

\* Performs the attach with both sides held: under way, the copy posted.
\* `attach_now` refuses a shard that stopped.
fair process attacher = "attacher"
begin
A_attach:
    if ~stopped then
        reg := "registering";
        ops := TRUE;
        tok := TRUE;
        mailbox := Append(mailbox, "attach");
    end if;
end process;

fair process closer = "closer"
begin
C_close:
    if Close then reqs := reqs \cup {"close"}; end if;
end process;

\* The shard going away: it stops, then evicts its sources.
fair process evicter = "evicter"
begin
E_stop:
    if Evict then stopped := TRUE; end if;
E_evict:
    if Evict then reqs := reqs \cup {"evict"}; end if;
end process;

\* Whoever holds both sides: every pending request at once, in the order
\* `perform` takes them (the close, the installs, the evict).
fair process performer = "performer"
begin
P_loop:
    await reqs # {} \/ (Quiet /\ pc["ring"] \in {"R_loop", "Done"});
    if reqs = {} then goto Done; end if;
P_perform:
    \* The close: `detach`, then the descriptor closes.
    if "close" \in reqs then
        \* MUTATION no_abandon: an attach under way is left to the ring.
        if reg = "registering" /\ ops /\ Mutation # "no_abandon" then
            reg := "unreg" || ops := FALSE || tok := FALSE || fdOpen := FALSE || closed := TRUE;
        elsif reg = "registered" then
            reg := "unreg" || ops := FALSE || tok := FALSE || fdOpen := FALSE || closed := TRUE
                || mailbox := Append(mailbox, "detach");
        else
            fdOpen := FALSE || closed := TRUE;
        end if;
        reqs := reqs \ {"close"};
    elsif "installed" \in reqs then
        \* `take_installs`: kept if the attach is still under way and the handle open.
        if reg = "registering" /\ ops /\ tok /\ fdOpen then
            reg := "registered";
        else
            mailbox := Append(mailbox, "detach");
            rejected := TRUE;
        end if;
        installed := FALSE;
        reqs := reqs \ {"installed"};
    elsif "evict" \in reqs then
        \* `evict_now`: an attach under way is undone, a registration goes.
        if reg = "registering" /\ ops /\ Mutation # "no_abandon" then
            reg := "unreg" || ops := FALSE || tok := FALSE || evicted := TRUE;
        elsif reg = "registered" then
            reg := "unreg" || ops := FALSE || tok := FALSE || evicted := TRUE
                || mailbox := Append(mailbox, "detach");
        else
            evicted := TRUE;
        end if;
        reqs := reqs \ {"evict"};
    end if;
    goto P_loop;
end process;

\* The ring's thread: its posted work, in order.
fair process ring = "ring"
variables msg = "none";
begin
R_loop:
    await mailbox # <<>> \/ (Quiet /\ pc["performer"] \in {"P_loop", "Done"});
    if mailbox = <<>> then
        goto Done;
    else
        msg := Head(mailbox);
        mailbox := Tail(mailbox);
    end if;
R_take:
    if msg = "detach" then
        table := FALSE;
        goto R_loop;
    end if;
R_look:
    \* `ring_attached`: a registration that was undone gets nothing.
    if ~(reg = "registering" /\ tok) then
        staleDrop := TRUE;
        goto R_loop;
    end if;
R_install:
    table := TRUE;
R_hand:
    \* MUTATION direct: the ring's thread writes the registration itself, as
    \* the code did before, without the sides.
    if Mutation = "direct" then
        reg := "registered";
    else
        installed := TRUE;
        reqs := reqs \cup {"installed"};
    end if;
    goto R_loop;
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

AllDone == \A p \in DOMAIN pc : pc[p] = "Done"

\* Once everything is done: a closed source leaves no file in the table and
\* is not registered; an evicted one is not registered.
NoLeak == AllDone => /\ (closed => ~table /\ reg # "registered")
                     /\ (evicted => reg # "registered")

\* The table holds the file exactly while the source is registered with the
\* ring or an install of it is on its way to the source.
TableFollows == AllDone => (table <=> reg = "registered")

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* The attach completes: the source ends registered.
NoRegistered == reg # "registered"
\* The ring finds the attach undone and puts nothing into its table.
NoStaleDrop == ~staleDrop
\* An install the source no longer wanted goes back to the ring.
NoRejected == ~rejected
=============================================================================

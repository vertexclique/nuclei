------------------------------- MODULE Attach -------------------------------
(***************************************************************************)
(* An attach of a source to a ring performed off the ring's thread, and a  *)
(* close or an evict meanwhile (src/io/source/requests.rs, `perform`,      *)
(* `ring_installed`, `take_installs`; src/io/source/ring.rs,                *)
(* `attach_ring`, `ring_attached`), at the grain of the requests performed *)
(* with both sides held and of the ring thread's steps.                    *)
(*                                                                          *)
(* In plain words: a thread that is not the ring's performs the attach: it *)
(* marks the registration under way and posts the ring a lease on the      *)
(* handle's object (no copy of the descriptor is made): the object stays   *)
(* open, its number the handle's, until the ring's thread is done with the *)
(* attach, however the handle closes meanwhile. The ring's thread looks    *)
(* whether the registration still wants it, puts the file into its table,  *)
(* hands the slot to the source as a request, and lets the lease go.      *)
(* Whoever performs that request, with both sides                          *)
(* held, keeps the slot if the attach is still under way and the handle    *)
(* open, else posts the ring to let it go. A close or an evict, performed  *)
(* the same way, undoes an attach under way. The file must never stay in   *)
(* the table of a source that closed, and a source the shard evicted must  *)
(* never end registered there. After the evict, the source may attach to  *)
(* another shard, B, under a token of the same number (every shard        *)
(* numbers its own): the ring's look and the install's taking check the   *)
(* registration's shard with its token, so the stale attach still in the *)
(* ring's mailbox never hands the source a slot of the wrong table.        *)
(*                                                                          *)
(* What is modelled step by step: the attach and every request (each one  *)
(* atomic step, as the sides make them); the ring's look, its install and *)
(* its hand-over as separate steps; the ring's mailbox, in order. What is *)
(* abstract: one source, one attach to the ring and at most one to B,     *)
(* whose ring's own look and install are one step.                        *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    Close,     \* TRUE: the handle closes at some point
    Evict,     \* TRUE: the shard evicts the source at some point
    Again,     \* TRUE: after the evict, the source attaches to shard B
    Mutation   \* "none", or the name of the one rule this run takes away

Requesters == {"attacher", "closer", "evicter"}

(* --algorithm Attach
variables
    reg = "unreg",      \* the registration: "unreg", "registering", "registered"
    ops = FALSE,        \* registered with a ring (or under way to)
    token = "none",     \* the registration's token; both shards' first is "t0"
    home = "none",      \* the shard the registration is with: "ring" (this one) or "B"
    slotFrom = "none",  \* the table the slot the source took is of
    fdOpen = TRUE,      \* the handle's descriptor is open
    leased = FALSE,     \* the attach in the ring's mailbox holds a lease on the object
    table = FALSE,      \* the ring's table holds the file
    mailbox = <<>>,     \* the ring's posted work, in order
    stopped = FALSE,    \* the shard is going away (set before it evicts its sources)
    reqs = {},          \* requests raised and not performed
    installed = FALSE,  \* an install handed to the source and not taken
    closed = FALSE,     \* (record) the close was performed
    evicted = FALSE,    \* (record) the evict was performed
    staleDrop = FALSE,  \* (record) the ring found the attach undone and put nothing in
    rejected = FALSE,   \* (record) an install the source no longer wanted went back
    installedClosed = FALSE; \* (record) the ring put the file of a closed descriptor into its table

define
    RequestersDone == \A p \in Requesters : pc[p] = "Done"
    Quiet == RequestersDone /\ mailbox = <<>> /\ reqs = {}
    \* The attach to this ring is the registration's: its token, with this
    \* shard. MUTATION token_only: the token alone.
    Ours == token = "t0" /\ (home = "ring" \/ Mutation = "token_only")
    \* The descriptor after the close: open while the attach's lease is out.
    \* MUTATION no_lease: the attach holds no lease, so the close closes it at once.
    KeptOpen == leased /\ Mutation # "no_lease"
end define;

\* Performs the attach with both sides held: under way, a lease posted.
\* `attach_now` refuses a shard that stopped, and a handle that closed (its
\* descriptor out of the source).
fair process attacher = "attacher"
begin
A_attach:
    if ~stopped /\ ~closed then
        reg := "registering";
        ops := TRUE;
        token := "t0";
        home := "ring";
        leased := TRUE;
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
            reg := "unreg" || ops := FALSE || token := "none" || home := "none"
                || fdOpen := KeptOpen || closed := TRUE;
        elsif reg = "registered" then
            reg := "unreg" || ops := FALSE || token := "none" || home := "none"
                || fdOpen := KeptOpen || closed := TRUE || mailbox := Append(mailbox, "detach");
        else
            fdOpen := KeptOpen || closed := TRUE;
        end if;
        reqs := reqs \ {"close"};
    elsif "installed" \in reqs then
        \* `take_installs`: kept if the attach is still under way and the handle open.
        if reg = "registering" /\ ops /\ Ours /\ ~closed then
            reg := "registered" || slotFrom := "ring";
        else
            mailbox := Append(mailbox, "detach");
            rejected := TRUE;
        end if;
        installed := FALSE;
        reqs := reqs \ {"installed"};
    elsif "evict" \in reqs then
        \* `evict_now`: an attach under way is undone, a registration goes.
        if reg = "registering" /\ ops /\ Mutation # "no_abandon" then
            reg := "unreg" || ops := FALSE || token := "none" || home := "none"
                || evicted := TRUE;
        elsif reg = "registered" then
            reg := "unreg" || ops := FALSE || token := "none" || home := "none"
                || evicted := TRUE || mailbox := Append(mailbox, "detach");
        else
            evicted := TRUE;
        end if;
        reqs := reqs \ {"evict"};
    end if;
    goto P_loop;
end process;

\* A task on shard B polls the source once the evict undid its registration
\* here, and attaches it there off B's thread, under a token of the same
\* number; B's ring then installs the file in its own table, and the source
\* takes that slot if the attach to B is still under way.
fair process reattacher = "reattacher"
begin
B_attach:
    await ~Again \/ (evicted /\ reg = "unreg");
    if Again /\ ~closed then
        reg := "registering" || ops := TRUE || token := "t0" || home := "B";
    end if;
B_install:
    if reg = "registering" /\ token = "t0" /\ home = "B" then
        reg := "registered" || slotFrom := "B";
    end if;
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
    if ~(reg = "registering" /\ Ours) then
        staleDrop := TRUE;
        goto R_unlease;
    end if;
R_install:
    \* The table takes the file of the descriptor the lease keeps open; a close
    \* may have come since the look.
    if ~fdOpen then installedClosed := TRUE; end if;
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
R_unlease:
    \* The attach's lease goes: a handle that closed meanwhile closes its object now.
    leased := FALSE;
    if closed then fdOpen := FALSE; end if;
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
NoLeak == AllDone => /\ (closed => ~table /\ reg # "registered" /\ ~fdOpen)
                     /\ (evicted => ~(reg = "registered" /\ home = "ring"))

\* The ring never puts the file of a descriptor that closed (whose number another file may
\* have taken) into its table.
NoClosedInstall == ~installedClosed

\* The table holds the file exactly while the source is registered with the
\* ring or an install of it is on its way to the source.
TableFollows == AllDone => (table <=> (reg = "registered" /\ home = "ring"))

\* A source registered with B never holds a slot of this ring's table.
NoForeignSlot == ~(reg = "registered" /\ home = "B" /\ slotFrom = "ring")

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
\* After the evict, the source ends registered with B.
NoRegisteredWithB == ~(reg = "registered" /\ home = "B")
=============================================================================

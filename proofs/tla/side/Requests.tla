----------------------------- MODULE Requests -----------------------------
(***************************************************************************)
(* A request that needs both sides of a source: a dropped handle's close   *)
(* (src/io/source/requests.rs), against a holder of each side.             *)
(*                                                                          *)
(* In plain words: each side's word has a busy bit and a request bit. The  *)
(* handle is dropped: its close goes into the source's request word, and  *)
(* the dropper takes the read side with one fetch_or that also raises the  *)
(* request bit. Taking it, it tries the write side the same way; with both *)
(* it performs the close itself (the descriptor is deregistered and       *)
(* closed) and drops its object. Taking fewer, it lets go of what it took  *)
(* and hands its object to the source, never waiting. Meanwhile a holder  *)
(* on each side takes its side, loads the descriptor, uses it, and lets   *)
(* go. Letting go is one compare-and-swap from the exact word the holder   *)
(* took to free; when another raised the request bit meanwhile it fails,   *)
(* and the holder takes the bits with one swap (still holding). Sides are  *)
(* taken in one order only, the read side first: a holder of the read side *)
(* tries the write side, with one fetch_or, and performs if it gets it;   *)
(* a holder of the write side lets go first, then tries the read side once *)
(* (getting it, it acts as a read-side holder). Whoever performs the close *)
(* marks it done; the object is dropped by whichever of the dropper's     *)
(* hand-over and the performer's mark comes second.                        *)
(*                                                                          *)
(* The close lingers: once the handle is gone, a task drains what it wrote *)
(* through a copy of the descriptor (or, with none to spare for a copy,   *)
(* through the handle's own object, kept open until it is done), as a     *)
(* holder of the write side. If it                                        *)
(* finds the close not performed yet, it waits on the write side for a    *)
(* wake; the performer wakes the write side's waiters (the handle's other *)
(* waiters are of operations that are gone, and are forgotten instead).   *)
(*                                                                          *)
(* What is abstract: the other flags of a side's word (Side.tla checks    *)
(* them), and the descriptor (open until its object is dropped).           *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    Mutation   \* "none", or the name of the one rule this run takes away

Sides == {"read", "write"}
Holders == {"hr", "hw"}
SideOf(h) == IF h = "hr" THEN "read" ELSE "write"
Other(s) == IF s = "read" THEN "write" ELSE "read"

(* --algorithm Requests
variables
    busy = [s \in Sides |-> FALSE],
    req = [s \in Sides |-> FALSE],
    pending = FALSE,          \* the source's request word holds the close
    performed = 0,            \* how many times the close was performed
    doomed = "empty",         \* the hand-over: "empty", "stored" or "closed"
    objectDropped = FALSE,    \* the handle's object, and so the descriptor, is closed
    loaded = [h \in Holders |-> FALSE],  \* a holder loaded the descriptor and will use it
    usedClosed = FALSE,       \* (record) a holder used a descriptor that was closed
    byHolder = FALSE,         \* (record) a holder performed the close
    handedOver = FALSE,       \* (record) the dropper handed its object over
    lingerWaits = FALSE,      \* the lingering drain waits on the write side for a wake
    lingerWaited = FALSE;     \* (record) it waited, before the close was performed

define
    \* The close, done with both sides held: deregistration and the descriptor out of the source.
    \* Nothing is left to happen but the lingering drain's wait for its wake.
    Settled == /\ \A p \in Holders \cup {"drop"} : pc[p] = "Done"
               /\ pc["linger"] \in {"L_wait", "Done"}
end define;

macro Perform() begin
    pending := FALSE;
    performed := performed + 1;
    \* The performer marks the close done; it drops the object if it was handed over.
    if doomed = "stored" then objectDropped := TRUE; end if;
    doomed := "closed";
    \* The write side's waiters are woken: a waiting drain goes on.
    \* MUTATION linger_forgets: they are forgotten, as the handle's others are.
    if Mutation # "linger_forgets" then lingerWaits := FALSE; end if;
end macro;

\* Holding the read side: tries the write side (one fetch_or that raises its request bit); getting it,
\* performs the close and lets it go.
procedure TryWrite()
begin
TW_take:
    if pending then
        if ~busy["write"] then
            busy["write"] := TRUE || req["write"] := TRUE;
TW_perform:
            Perform();
            byHolder := byHolder \/ self # "drop";
TW_release:
            call Release("write");
            return;
        else
            req["write"] := TRUE;
        end if;
    end if;
TW_done:
    return;
end procedure;

\* Lets go of `side`: one compare-and-swap from the word the holder took; when a request bit came, the
\* bits are taken (the side still held) and the request is looked at: from the read side by trying the
\* write side, from the write side by trying the read side once the write side is let go.
procedure Release(side)
variables sawReq = FALSE;
begin
Rel_cas:
    \* MUTATION expect_with_req: the holder takes a raised request bit for its own and lets go.
    if ~req[side] \/ Mutation = "expect_with_req" then
        busy[side] := FALSE || req[side] := FALSE;
        goto Rel_after;
    else
        req[side] := FALSE;
        sawReq := TRUE;
    end if;
Rel_req:
    \* MUTATION release_ignores_req: the request bit is cleared and nothing looked at.
    if side = "read" /\ Mutation # "release_ignores_req" then
        call TryWrite();
    elsif side = "write" /\ Mutation = "write_holder_takes_read" then
        \* MUTATION write_holder_takes_read: the write side's holder tries the read side while it
        \* still holds the write side, the two holders each raising the bit at the other.
        if ~busy["read"] then
            busy["read"] := TRUE || req["read"] := TRUE;
            pending := FALSE;
            performed := performed + 1;
            if doomed = "stored" then objectDropped := TRUE; end if;
            doomed := "closed";
Rel_mut_release:
            call Release("read");
        else
            req["read"] := TRUE;
        end if;
    end if;
Rel_again:
    goto Rel_cas;
Rel_after:
    \* Let go of the write side: one try at the read side, as its holder.
    if side = "write" /\ sawReq /\ pending /\ Mutation # "release_ignores_req" then
        if ~busy["read"] then
            busy["read"] := TRUE || req["read"] := TRUE;
Rel_as_reader:
            call TryWrite();
Rel_reader_release:
            call Release("read");
            return;
        else
            req["read"] := TRUE;
        end if;
    end if;
Rel_end:
    return;
end procedure;

fair process dropper = "drop"
begin
D_pend:
    pending := TRUE;
D_take_read:
    \* MUTATION perform_with_one_side: the dropper performs with the read side alone.
    if ~busy["read"] then
        busy["read"] := TRUE || req["read"] := TRUE;
        if Mutation = "perform_with_one_side" then
            Perform();
D_rel_mut:
            call Release("read");
            goto D_own;
        end if;
D_try_write:
        call TryWrite();
D_rel_read:
        call Release("read");
    else
        req["read"] := TRUE;
    end if;
D_check:
    if doomed = "closed" /\ ~handedOver then
        \* Performed by the dropper itself (nothing handed over yet): it drops its own object.
        goto D_own;
    end if;
D_hand:
    \* MUTATION drop_closes_at_once: the object is dropped even though the close is not done.
    if Mutation = "drop_closes_at_once" then
        objectDropped := TRUE;
    else
        handedOver := TRUE;
        if doomed = "closed" then objectDropped := TRUE; end if;
        doomed := IF doomed = "closed" THEN "closed" ELSE "stored";
    end if;
    goto Done;
D_own:
    objectDropped := TRUE;
end process;

\* The lingering drain, spawned once the dropper is done: a holder of the write side that waits for a
\* wake when the close is not performed yet.
fair process lingerer = "linger"
begin
L_start:
    await pc["drop"] = "Done";
L_take:
    await ~busy["write"];
    busy["write"] := TRUE;
L_drain:
    if performed = 0 then
        lingerWaits := TRUE;
        lingerWaited := TRUE;
    end if;
L_release:
    call Release("write");
L_wait:
    await ~lingerWaits;
end process;

fair process holder \in Holders
begin
H_take:
    await ~busy[SideOf(self)];
    busy[SideOf(self)] := TRUE;
H_load:
    \* The driver's part loads the descriptor; a closed source has none to load.
    loaded[self] := doomed # "closed";
H_use:
    if loaded[self] /\ objectDropped then usedClosed := TRUE; end if;
    loaded[self] := FALSE;
H_release:
    call Release(SideOf(self));
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

\* No holder uses a descriptor that was closed under it.
NoUseOfClosed == ~usedClosed

\* The close is performed once at most.
PerformedOnce == performed <= 1

\* Once everything else is done, the close was performed and the object dropped.
Closed == Settled => (performed = 1 /\ objectDropped)

\* `Termination` (every process ends) is the one the translator writes for the fair processes.

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A holder, not the dropper, performs the close.
NoHolderClose == ~byHolder
\* The dropper hands its object over instead of dropping it.
NoHandOver == ~handedOver
\* The lingering drain waits before the close is performed (and so needs the performer's wake).
NoLingerWait == ~lingerWaited
=============================================================================

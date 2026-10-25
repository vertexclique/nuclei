------------------------------- MODULE Watch -------------------------------
(***************************************************************************)
(* The watch channel (src/sync/watch.rs), at the grain of its atomic       *)
(* steps.                                                                  *)
(*                                                                          *)
(* In plain words: the value lives in a kovan `Atom`, and a state word     *)
(* holds a version (one per publication) and a closed bit. A send stores   *)
(* the value, then bumps the version, then wakes the waiting receivers. A  *)
(* modifying send loads the value, changes a copy, and publishes it with a *)
(* compare-and-swap, starting over if another sender published first. The *)
(* last sender to go sets the closed bit and wakes the receivers. A        *)
(* receiver's `changed` makes its `Notified` first, then reads the state:  *)
(* a version it has not seen is its answer, else the closed bit is, else  *)
(* it waits for the next wake. A borrow reads the version, then the value. *)
(* A sender's `closed` waits for the receivers to go the same way: its     *)
(* `Notified` first, then the count of receivers.                          *)
(*                                                                          *)
(* What is modelled step by step: the store or compare-and-swap of the     *)
(* value, the version bump, each wake, the close, the receiver counts and  *)
(* their wake, and each read of the state, the value and the count. What   *)
(* is abstract: a value is its store order and a count of the changes made *)
(* to it; `Notify` is one counter of `notify_waiters` calls per side, and a *)
(* `Notified` made when it stood at `g` completes once it passes `g`,      *)
(* which is what `Notify` promises (its own model is the notify family);   *)
(* the `Atom`'s reclamation is kovan's, checked there.                     *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    SendOps,      \* one sequence per sender of "send" (store a new value) or "modify" (send_if_modified)
    WithChanged,  \* a receiver waits for changes until the close
    WithBorrower, \* a receiver borrows once, then goes
    WithClosed,   \* one more sender waits until every receiver went, then goes
    Mutation      \* "none", or the name of the one rule this run takes away

Senders == 1..Len(SendOps)
ChangedT == 10
BorrowT == 11
ClosedT == 12
TotalOps == LET Sum[i \in 0..Len(SendOps)] ==
                    IF i = 0 THEN 0 ELSE Sum[i - 1] + Len(SendOps[i])
            IN Sum[Len(SendOps)]

(* --algorithm Watch
variables
    valIdx = 0,       \* the value in the Atom: its store order (0: the initial one)
    valCount = 0,     \* and the changes made to it by modifying sends
    stores = 0,       \* values stored so far
    version = 0,      \* the state word's version (publications counted)
    closed = FALSE,   \* the state word's closed bit
    ngen = 0,         \* notify_waiters calls on the receivers' Notify
    tgen = 0,         \* notify_waiters calls on the producer's Notify
    txCount = Len(SendOps) + (IF WithClosed THEN 1 ELSE 0),
    rxCount = (IF WithChanged THEN 1 ELSE 0) + (IF WithBorrower THEN 1 ELSE 0),
    modifies = 0,     \* modifying sends that published
    \* records
    errUnseen = FALSE,   \* `changed` answered closed with a version it had not seen
    pairedOlder = FALSE, \* a borrow paired a version with an older value
    ahead = FALSE,       \* a borrow's value was newer than its version (harmless)
    retried = FALSE,     \* a compare-and-swap lost to another sender
    changesSeen = 0;     \* versions `changed` answered with

define
    SendersDone == \A s \in Senders : pc[s] = "Done"
    \* Safety.
    ErrOnlyWhenSeen == ~errUnseen
    PairedNoOlder == ~pairedOlder
    VersionCountsPublications == SendersDone => version = TotalOps
    NoLostModify == SendersDone => valCount = modifies
    \* Liveness.
    ReceiverEnds == WithChanged => <>(pc[ChangedT] = "Done")
    ProducerEnds == WithClosed => <>(pc[ClosedT] = "Done")
    \* Witnesses.
    NoChangeSeen == changesSeen = 0
    NoRetry == ~retried
    NeverAhead == ~ahead
end define;

macro closeIfLast(isLast) begin
    if isLast then
        closed := TRUE;
    end if;
end macro;

fair process sender \in Senders
variables i = 1, curIdx = 0, curCount = 0, last = FALSE;
begin
S_op:
    while i <= Len(SendOps[self]) do
        if SendOps[self][i] = "send" then
S_store:
            stores := stores + 1;
            valIdx := stores;
            valCount := 0;
        else
S_load:
            curIdx := valIdx;
            curCount := valCount;
S_cas:
            \* The guard keeps the loaded value from reuse: comparing its
            \* store order is comparing the pointer.
            if Mutation = "store_not_cas" \/ valIdx = curIdx then
                stores := stores + 1;
                valIdx := stores;
                valCount := curCount + 1;
                modifies := modifies + 1;
            else
                retried := TRUE;
                goto S_load;
            end if;
        end if;
S_bump:
        version := version + 1;
S_notify:
        ngen := ngen + 1;
        i := i + 1;
    end while;
S_drop:
    last := txCount = 1;
    txCount := txCount - 1;
S_close:
    closeIfLast(last);
S_closeNotify:
    if last then
        ngen := ngen + 1;
    end if;
end process;

fair process changed \in (IF WithChanged THEN {ChangedT} ELSE {})
variables seen = 0, myGen = 0, stV = 0, stC = FALSE, lastRx = FALSE;
begin
R_start:
    if Mutation = "read_before_notified" then
        goto R_readFirst;
    end if;
R_create:
    myGen := ngen;
R_read:
    stV := version;
    stC := closed;
    goto R_decide;
R_readFirst:
    stV := version;
    stC := closed;
R_createLate:
    myGen := ngen;
R_decide:
    if Mutation = "closed_first" /\ stC then
        errUnseen := errUnseen \/ stV # seen;
        goto R_drop;
    elsif stV # seen then
        seen := stV;
        changesSeen := changesSeen + 1;
        goto R_start;
    elsif stC then
        goto R_drop;
    end if;
R_wait:
    await ngen > myGen;
    goto R_start;
R_drop:
    lastRx := rxCount = 1;
    rxCount := rxCount - 1;
R_dropNotify:
    if lastRx then
        tgen := tgen + 1;
    end if;
end process;

fair process borrower \in (IF WithBorrower THEN {BorrowT} ELSE {})
variables bv = 0, bidx = 0, lastB = FALSE;
begin
B_start:
    if Mutation = "value_first" then
        goto B_valueFirst;
    end if;
B_version:
    bv := version;
B_value:
    bidx := valIdx;
    goto B_check;
B_valueFirst:
    bidx := valIdx;
B_versionLate:
    bv := version;
B_check:
    \* Every publication the version counts stored a value before it, and
    \* the Atom holds the newest store: its order is at least the version.
    pairedOlder := pairedOlder \/ bidx < bv;
    ahead := ahead \/ bidx > bv;
B_drop:
    lastB := rxCount = 1;
    rxCount := rxCount - 1;
B_dropNotify:
    if lastB then
        tgen := tgen + 1;
    end if;
end process;

fair process producer \in (IF WithClosed THEN {ClosedT} ELSE {})
variables myT = 0, count = 0, lastP = FALSE;
begin
P_start:
    if Mutation = "count_before_notified" then
        goto P_countFirst;
    end if;
P_create:
    myT := tgen;
P_count:
    count := rxCount;
    goto P_decide;
P_countFirst:
    count := rxCount;
P_createLate:
    myT := tgen;
P_decide:
    if count = 0 then
        goto P_drop;
    end if;
P_wait:
    await tgen > myT;
    goto P_start;
P_drop:
    lastP := txCount = 1;
    txCount := txCount - 1;
P_close:
    closeIfLast(lastP);
P_closeNotify:
    if lastP then
        ngen := ngen + 1;
    end if;
end process;
end algorithm; *)

\* After the translation, which declares the threads' own variables: the
\* receiver that ended saw the last version.
SawTheLast == WithChanged /\ pc[ChangedT] = "Done" => seen[ChangedT] = version
=============================================================================

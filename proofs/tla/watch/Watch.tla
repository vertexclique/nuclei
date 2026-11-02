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
(* Also modelled: `send_replace` (`Sender::send_replace`), which swaps the *)
(* value, publishes it, and only then clones the old one, whose clone may  *)
(* panic and unwind the sender into its drop; `Receiver::wait_for`, which  *)
(* takes a snapshot (the version, then the value), judges the value, and   *)
(* otherwise waits through `changed`, judging nothing more after the close *)
(* unless the value changed; `Sender::subscribe`, which counts a new       *)
(* receiver at the version it loads; and `Sender::clone`, which counts the *)
(* new sender before the handle exists.                                    *)
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
    WithWaitFor,  \* a receiver runs wait_for(f) once, then goes
    FitAt,        \* f accepts a value whose store order is at least this
    Mutation      \* "none", or the name of the one rule this run takes away

Senders == 1..Len(SendOps)
ChangedT == 10
BorrowT == 11
ClosedT == 12
WaitForT == 13
SubT == 14
CloneT == 15
\* Some sender's operations include `o`.
HasOp(o) == \E s \in 1..Len(SendOps) : \E k \in 1..Len(SendOps[s]) : SendOps[s][k] = o
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
    rxCount = (IF WithChanged THEN 1 ELSE 0) + (IF WithBorrower THEN 1 ELSE 0)
              + (IF WithWaitFor THEN 1 ELSE 0),
    modifies = 0,     \* modifying sends that published
    \* records
    errUnseen = FALSE,   \* `changed` answered closed with a version it had not seen
    pairedOlder = FALSE, \* a borrow paired a version with an older value
    ahead = FALSE,       \* a borrow's value was newer than its version (harmless)
    retried = FALSE,     \* a compare-and-swap lost to another sender
    changesSeen = 0,     \* versions `changed` answered with
    subscribed = FALSE,  \* a sender subscribed the extra receiver
    subVer = 0,          \* the version that receiver starts from
    subAt = 0,           \* (record) the version as the subscribe ran
    subOld = FALSE,      \* (record) its `changed` answered with no publication since it subscribed
    cloned = FALSE,      \* a sender cloned itself into the extra sender
    sentPastClose = FALSE, \* (record) a publication after the close
    wfErrBad = FALSE;    \* (record) wait_for answered closed without judging the newest value

define
    SendersDone == \A s \in Senders : pc[s] = "Done"
    \* Every sender, the clone included, is done.
    AllSendersDone == SendersDone /\ (HasOp("clone") => pc[CloneT] = "Done")
    \* Every value stored was published: a version bump and a wake for each one.
    StoresAnnounced == AllSendersDone => version = stores
    \* Nothing is published after the last sender went.
    NoSendPastClose == ~sentPastClose
    \* wait_for answers closed only once f turned down the newest value.
    WaitForErrOnNewest == ~wfErrBad
    \* A subscribed receiver's `changed` answers only a publication made since it subscribed.
    SubSeesOnlyNewer == ~subOld
    \* Safety.
    ErrOnlyWhenSeen == ~errUnseen
    PairedNoOlder == ~pairedOlder
    VersionCountsPublications == SendersDone => version = TotalOps
    NoLostModify == SendersDone => valCount = modifies
    \* Liveness.
    ReceiverEnds == WithChanged => <>(pc[ChangedT] = "Done")
    ProducerEnds == WithClosed => <>(pc[ClosedT] = "Done")
    WaitForEnds == WithWaitFor => <>(pc[WaitForT] = "Done")
    SubEnds == HasOp("subscribe") => <>(pc[SubT] = "Done")
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
        elsif SendOps[self][i] \in {"replace", "replace_panic"} then
S_swap:
            \* `send_replace`: one swap of the value; the old one comes back to be cloned.
            stores := stores + 1;
            valIdx := stores;
            valCount := 0;
            \* MUTATION replace_clone_first: the old value is cloned before the publish, and
            \* the clone panics there: the sender unwinds into its drop with the new value
            \* stored and never published.
            if SendOps[self][i] = "replace_panic" /\ Mutation = "replace_clone_first" then
                goto S_drop;
            end if;
        elsif SendOps[self][i] = "subscribe" then
S_sub:
            \* `subscribe`: one load of the state for the new receiver's version, and the
            \* receiver count up by one.
            \* MUTATION subscribe_from_zero: the new receiver starts from version zero.
            subVer := IF Mutation = "subscribe_from_zero" THEN 0 ELSE version;
            subAt := version;
            rxCount := rxCount + 1;
            subscribed := TRUE;
            goto S_skip;
        elsif SendOps[self][i] = "clone" then
S_clone:
            \* `Sender::clone`: the sender count up by one, then the new handle exists.
            \* MUTATION clone_counts_late: the new handle counts itself only once it runs.
            if Mutation # "clone_counts_late" then
                txCount := txCount + 1;
            end if;
            cloned := TRUE;
            goto S_skip;
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
        sentPastClose := sentPastClose \/ closed;
S_notify:
        ngen := ngen + 1;
        \* A `send_replace` clones the old value now; a clone that panics unwinds the
        \* sender into its drop, after the publish.
        if SendOps[self][i] = "replace_panic" then
            goto S_drop;
        else
            i := i + 1;
        end if;
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
    goto Done;
S_skip:
    \* An operation that publishes nothing: on to the next one.
    i := i + 1;
    goto S_op;
end process;

\* The sender a `clone` made: once it exists it sends once, then goes.
fair process clone \in (IF HasOp("clone") THEN {CloneT} ELSE {})
variables lastC = FALSE;
begin
C_wait:
    await cloned;
C_count:
    \* MUTATION clone_counts_late: the count goes up only now.
    if Mutation = "clone_counts_late" then
        txCount := txCount + 1;
    end if;
C_store:
    stores := stores + 1;
    valIdx := stores;
    valCount := 0;
C_bump:
    version := version + 1;
    sentPastClose := sentPastClose \/ closed;
C_notify:
    ngen := ngen + 1;
C_drop:
    lastC := txCount = 1;
    txCount := txCount - 1;
C_close:
    closeIfLast(lastC);
C_closeNotify:
    if lastC then
        ngen := ngen + 1;
    end if;
end process;

\* The receiver a `subscribe` made: once it exists, one `changed`, then it goes.
fair process sub \in (IF HasOp("subscribe") THEN {SubT} ELSE {})
variables uVer = 0, uGen = 0, uV = 0, uC = FALSE, lastU = FALSE;
begin
U_wait:
    await subscribed;
    uVer := subVer;
U_create:
    \* `changed`: the Notified first, then the state.
    uGen := ngen;
U_read:
    uV := version;
    uC := closed;
U_decide:
    if uV # uVer then
        subOld := subOld \/ uV = subAt;   \* (record) a change answered that is not one
        goto U_drop;
    elsif uC then
        goto U_drop;
    end if;
U_sleep:
    await ngen > uGen;
    goto U_create;
U_drop:
    lastU := rxCount = 1;
    rxCount := rxCount - 1;
U_dropNotify:
    if lastU then
        tgen := tgen + 1;
    end if;
end process;

\* A receiver that runs `wait_for(f)`, f accepting a store order of at least FitAt.
fair process waitFor \in (IF WithWaitFor THEN {WaitForT} ELSE {})
variables wVer = 0, wClosed = FALSE, sv = 0, sidx = 0, judged = 0, wGen = 0, wV = 0,
          wC = FALSE, lastW = FALSE;
begin
F_snap:
    \* `Shared::snapshot`: the version first, then the value.
    \* MUTATION wait_for_value_first: the value first, then the version.
    if Mutation = "wait_for_value_first" then
        sidx := valIdx;
    else
        sv := version;
    end if;
F_snap2:
    if Mutation = "wait_for_value_first" then
        sv := version;
    else
        sidx := valIdx;
    end if;
F_judge:
    \* f is called unless the channel closed and nothing changed since f last judged; the
    \* snapshot's version is marked seen either way.
    if (~wClosed \/ sv # wVer) /\ sidx >= FitAt then
        judged := sidx;
        wVer := sv;
        goto F_drop;               \* Ok, with the value f accepted
    elsif wClosed then
        \* Closed: Err, with the newest value judged (now or before) and turned down.
        wfErrBad := wfErrBad \/ (IF sv # wVer THEN sidx ELSE judged) # valIdx;
        judged := IF sv # wVer THEN sidx ELSE judged;
        wVer := sv;
        goto F_drop;
    else
        judged := sidx;            \* f turned it down: wait for a change
        wVer := sv;
    end if;
F_create:
    \* `changed`: the Notified first, then the state.
    wGen := ngen;
F_read:
    wV := version;
    wC := closed;
F_decide:
    if wV # wVer then
        wVer := wV;
        goto F_snap;               \* a change: judge again
    elsif wC then
        wClosed := TRUE;
        goto F_snap;               \* the close: one last look
    end if;
F_sleep:
    await ngen > wGen;
    goto F_create;
F_drop:
    lastW := rxCount = 1;
    rxCount := rxCount - 1;
F_dropNotify:
    if lastW then
        tgen := tgen + 1;
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

----------------------------- MODULE ReentrantRaw -----------------------------
(***************************************************************************)
(* A reentrant read against the plain lock's owed handoff and a dropped    *)
(* read (src/sync/reentrant_rwlock.rs: `ReentrantRead::poll`,              *)
(* `reenter_read`, `enter_read`, `Drop for ReentrantRead`; the plain lock *)
(* of src/sync/raw_rwlock.rs: `try_kind`, `RwWait::poll`, `RwWait::cancel` *)
(* and the handoff a waiter past the bypass bound is owed), one step per   *)
(* poll, release or drop.                                                  *)
(*                                                                          *)
(* In plain words: an owner that already reads reads again at once,        *)
(* taking no share of the plain lock, so it never queues behind a writer   *)
(* that is owed the lock (which waits for that very owner's read to end).  *)
(* Its first read takes a share: at once while no writer holds the lock    *)
(* and none is owed it, else it queues and a pass grants it once the       *)
(* writer is through. A read dropped while queued leaves the queue; one a  *)
(* pass granted just before the drop gives its share back. A writer that   *)
(* waits may come to be owed the lock: new reads then queue behind it, and *)
(* it takes the lock once no reader is inside.                             *)
(*                                                                          *)
(* Who acts: one owner (its reads, nested reads, releases and a dropped    *)
(* read) and one writer (two writes). What must never happen: the owner    *)
(* waiting while it holds a read; a share lost to a dropped read; either   *)
(* side left waiting for good.                                             *)
(*                                                                          *)
(* What is abstract: the plain lock is a reader count, a writer bit, the   *)
(* owed bit and who is queued (its own protocol is the rwlock family's);  *)
(* the owner's records are one depth count (the record list is            *)
(* ReentrantRwLock.tla's); the writer becomes owed at a moment of the      *)
(* model's choosing.                                                       *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    OwnerProg, \* the owner's steps: "R" a read, "r" let the newest read go, "D" a dropped read
    Mutation   \* "none", or the name of the one rule this run takes away

OwnerT == "owner"
WriterT == "writer"

(* --algorithm ReentrantRaw
variables
    readers = 0,          \* read shares of the plain lock
    writer = FALSE,       \* the plain lock's writer bit
    owedW = FALSE,        \* the writer is owed the lock: new reads queue
    wq = FALSE,           \* the writer is queued
    rq = FALSE,           \* the owner's read is queued
    rGranted = FALSE,     \* a pass granted the owner's queued read
    depth = 0,            \* the owner's read depth (its record)
    waitedHolding = FALSE; \* (record) the owner waited while holding a read

define
    \* A read share is free now: no writer inside, none owed it.
    ReadFree == ~writer /\ ~owedW
    \* The owner never waits while it holds a read.
    NoWaitWhileHolding == ~waitedHolding
    \* Both sides end.
    AllEnd == <>(pc[OwnerT] = "Done" /\ pc[WriterT] = "Done")
end define;

fair process owner = OwnerT
variables i = 1;
begin
O_next:
    if i > Len(OwnerProg) then
        goto Done;
    elsif OwnerProg[i] = "r" then
        \* The guard's drop: the last level gives the share back.
        depth := depth - 1;
        if depth = 0 then
            readers := readers - 1;
        end if;
        i := i + 1;
        goto O_next;
    elsif depth > 0 /\ OwnerProg[i] = "R" /\ Mutation # "reenter_takes_share" then
        \* `reenter_read`: this owner reads already, so it reads again at once, no share.
        \* MUTATION reenter_takes_share: a nested read takes a share like a first one.
        depth := depth + 1;
        i := i + 1;
        goto O_next;
    end if;
O_try:
    \* `try_kind(READ)`: a share at once if one is free; else the read queues.
    if ReadFree then
        \* A share at once ("D" got its share at once too: its guard goes at once, no change).
        if OwnerProg[i] = "R" then
            readers := readers + 1;
            depth := depth + 1;
        end if;
        i := i + 1;
        goto O_next;
    else
        waitedHolding := waitedHolding \/ depth > 0;
        rq := TRUE;
    end if;
O_wait:
    \* Queued: granted by a pass, or (for "D") dropped first.
    either
        await rGranted;
        rGranted := FALSE;
        if OwnerProg[i] = "R" then
            depth := depth + 1;
        else
            \* "D" dropped after the grant (`RwWait::cancel` answers Granted): the share back.
            \* MUTATION drop_keeps_grant: the share is kept.
            if Mutation # "drop_keeps_grant" then
                readers := readers - 1;
            end if;
        end if;
    or
        await OwnerProg[i] = "D" /\ rq;
        rq := FALSE;               \* withdrawn while queued
    end either;
    i := i + 1;
    goto O_next;
end process;

\* The writer: two writes, each waiting (and maybe owed) until no reader is inside.
fair process writerP = WriterT
variables w = 0;
begin
W_next:
    if w = 2 then
        goto Done;
    end if;
W_try:
    if ~writer /\ readers = 0 /\ ~rq then
        writer := TRUE;
        goto W_hold;
    else
        wq := TRUE;
    end if;
W_wait:
    \* Waiting, it may come to be owed the lock (past the bypass bound).
    either
        await ~owedW;
        owedW := TRUE;
        goto W_wait;
    or
        await readers = 0 /\ ~writer;
        writer := TRUE;
        wq := FALSE;
        owedW := FALSE;
    end either;
W_hold:
    \* The write's release, and the pass that grants a queued read.
    writer := FALSE;
    if rq then
        rq := FALSE;
        readers := readers + 1;
        rGranted := TRUE;
    end if;
    w := w + 1;
    goto W_next;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

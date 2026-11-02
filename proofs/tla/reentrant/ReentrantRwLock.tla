-------------------------- MODULE ReentrantRwLock --------------------------
(***************************************************************************)
(* The reentrant reader-writer lock (src/sync/reentrant_rwlock.rs:        *)
(* `reenter_read`, `enter_read`, `read_unlock`, `reenter_write`,          *)
(* `enter_write`, `write_unlock`, `Records::claim`, `ReentrantRead::poll`  *)
(* and `ReentrantWrite::poll`), at the grain of its atomic steps.          *)
(*                                                                          *)
(* In plain words: under it is the plain reader-writer lock. Each reading  *)
(* owner has a record (its id and its read depth) in a list that only      *)
(* grows: a record whose owner stopped reading is claimed again with a     *)
(* compare-and-swap of its id. An owner that already reads reads again at *)
(* once; an owner that writes may read too, and those reads take no share  *)
(* of the lock of their own; any other read takes a read share first. An  *)
(* owner that writes writes again at once; one that reads without writing *)
(* is refused the write (it would wait for its own read); any other takes  *)
(* the write. The last read of an owner gives its record up, and its share *)
(* back unless the owner writes. The last write of an owner clears the     *)
(* writer id, and downgrades the write to a read if the owner still reads, *)
(* else gives the write back.                                              *)
(*                                                                          *)
(* One owner may have two requests out at once (`join!`), queued behind    *)
(* another owner. Every poll of a request looks at its owner's write and   *)
(* record first, queued or not: a read whose owner came to write or read   *)
(* through the other request leaves the queue (passing on a share it was  *)
(* granted) and reads again; a write whose owner came to read is refused.  *)
(*                                                                          *)
(* What is modelled step by step: the writer id, the write depth, each     *)
(* record's owner and reads, each lookup, the claim's look and its        *)
(* compare-and-swap (which a racing claim can beat), the push, and the     *)
(* plain lock's shares. What is abstract: the plain lock is a count of     *)
(* read shares and a writer bit, with waits that end when they let the     *)
(* request in, and a downgrade in one step; its own protocol is the rwlock *)
(* family's. A claim's walk of the list may miss a record freed behind it, *)
(* so a claim may push a fresh record though a free one exists. A read     *)
(* unlock's fetch_sub and its clear of the owner are one step: between     *)
(* them only the owner itself can tell the record from a free one.         *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    Progs,     \* per owner, its steps: "R" / "W" take a read / write guard, "r" / "w" let the newest go;
               \* "JWR" joins a write and a read (the owner then holds both guards), "JRR" joins
               \* two reads (two read guards), "WQR" asks to write and, while that waits, reads
               \* (one read guard: the write is refused, or was taken at once and let go)
    Mutation   \* "none", or the name of the one rule this run takes away

Owners == 1..Len(Progs)
NoOwner == 0
MaxRecords == Len(Progs)

(* --algorithm ReentrantRwLock
variables
    rawReaders = 0,
    rawWriter = FALSE,
    writerId = NoOwner,
    writeDepth = 0,
    recOwner = <<>>,          \* the records by place (a place stands for an address): owners (NoOwner: free)
    recReads = <<>>,          \* and their reads
    rguards = [o \in Owners |-> 0],
    wguards = [o \in Owners |-> 0],
    rshare = [o \in Owners |-> FALSE],   \* the owner holds a read share of the plain lock
    wshare = [o \in Owners |-> FALSE],   \* the owner holds the plain lock's write
    granted = 0,                         \* read shares a pass granted to a queued read, not taken up
    refused = [o \in Owners |-> FALSE],  \* (record) a write was refused to a reading owner
    downgraded = FALSE,                  \* (record) a last write became its owner's read
    reused = FALSE,                      \* (record) a claim took a record freed before
    lostClaim = FALSE;                   \* (record) a claim's compare-and-swap was beaten

define
    Find(id) == {x \in DOMAIN recOwner : recOwner[x] = id}
    Free == Find(NoOwner)
    ReadsOf(o) == IF Find(o) = {} THEN 0 ELSE recReads[CHOOSE x \in Find(o) : TRUE]
    \* An owner that writes holds the lock alone.
    Exclusion == \A o \in Owners : wguards[o] > 0 =>
                     \A p \in Owners \ {o} : rguards[p] = 0 /\ wguards[p] = 0
    \* The plain lock's shares are exactly the owners'.
    SharesMatch == rawReaders = Cardinality({o \in Owners : rshare[o]}) + granted
                   /\ rawWriter = (\E o \in Owners : wshare[o])
                   /\ Cardinality({o \in Owners : wshare[o]}) <= 1
    \* An owner with guards holds a share that covers them.
    GuardsCovered == \A o \in Owners :
                         /\ wguards[o] > 0 => wshare[o]
                         /\ rguards[o] > 0 => rshare[o] \/ wshare[o]
    \* One record per reading owner, and its reads are the owner's read guards.
    OneRecordEach == \A o \in Owners : Cardinality(Find(o)) <= 1
    ReadsMatch == \A o \in Owners : ReadsOf(o) = rguards[o]
    AllDone == \A o \in Owners : pc[o] = "Done"
    FreeAtEnd == AllDone => rawReaders = 0 /\ ~rawWriter /\ writerId = NoOwner
    AllOwnersEnd == <>AllDone
    \* Witnesses.
    NoRefusal == \A o \in Owners : ~refused[o]
    NoDowngrade == ~downgraded
    NoReuse == ~reused
    NoLostClaim == ~lostClaim
end define;

\* `enter_read`: claim a free record, or push a new one; reads := 1 (and the guard).
procedure EnterRead()
variables slot = 0;
begin
E_look:
    \* The walk: a record it saw free, or none (it may miss one freed behind it).
    either
        with x \in Free do
            slot := x;
        end with;
    or
        slot := 0;
    end either;
E_claim:
    if slot = 0 then
        \* A fresh record, pushed with a compare-and-swap loop.
        recOwner := Append(recOwner, self);
        recReads := Append(recReads, 0);
        slot := Len(recOwner);
    elsif recOwner[slot] = NoOwner \/ Mutation = "claim_without_cas" then
        recOwner[slot] := self;
        reused := TRUE;
    else
        \* Another claim took it first: walk on.
        lostClaim := TRUE;
        goto E_look;
    end if;
E_reads:
    recReads[slot] := 1;
    rguards[self] := rguards[self] + 1;
    return;
end procedure;

fair process owner \in Owners
variables i = 1, st = "R", queuedJ = FALSE;
begin
O_step:
    while i <= Len(Progs[self]) do
        st := Progs[self][i];
        if st = "JWR" then
JW_look:
            \* The join's first pass: the write, taken at once if nobody holds the lock; else
            \* both requests queue behind the other owner.
            if ~rawWriter /\ rawReaders = 0 then
                rawWriter := TRUE;                  \* the plain lock's write, at once
                wshare[self] := TRUE;               \* the owner's
                queuedJ := FALSE;                   \* the read has not queued
                goto JW_enter;
            else
                queuedJ := TRUE;                    \* both queue
            end if;
JW_wait:
            \* The task sleeps until the write is handed to the older request.
            await ~rawWriter /\ rawReaders = 0;   \* the write's grant
            rawWriter := TRUE;                      \* the plain lock's write
            wshare[self] := TRUE;                   \* the owner's
JW_enter:
            \* `enter_write`: the depth is one ...
            writeDepth := 1;                        \* one level
            wguards[self] := wguards[self] + 1;     \* the write guard
JW_store:
            \* ... then the writer id.
            writerId := self;                       \* the owner writes now
JW_read:
            \* The same pass polls the read: its owner writes, so it leaves the queue (no read
            \* share can have been granted beside the write) and reads at once. MUTATION
            \* queued_read_skips_reenter: a queued read only looks at its node and sleeps on.
            if queuedJ /\ Mutation = "queued_read_skips_reenter" then
                goto JW_readWait;
            else
                call EnterRead();                   \* `enter_read`: its record, no share of its own
                goto O_next;
            end if;
JW_readWait:
            \* The read waits for a share, which the owner's own write keeps out, and the join
            \* keeps the write until the read ends.
            await ~rawWriter;                       \* never, while the owner writes
            call EnterRead();                       \* (unreachable)
            goto O_next;
        elsif st = "JRR" then
JR_look:
            \* The join's first pass: the older read takes a share at once if no writer holds the
            \* lock; else both queue behind the other owner's write.
            if ~rawWriter then
                rawReaders := rawReaders + 1;       \* the older's share, at once
                rshare[self] := TRUE;               \* the owner's
                queuedJ := FALSE;                   \* the younger has not queued
                goto JR_older;
            else
                queuedJ := TRUE;                    \* both queue
            end if;
JR_wait:
            \* The write's unlock grants both queued reads in one pass.
            await ~rawWriter;                       \* the grant
            rawReaders := rawReaders + 2;           \* a share for each read
            rshare[self] := TRUE;                   \* the older's is the owner's
            granted := granted + 1;                 \* the younger's is not taken up yet
JR_older:
            \* The older read's `enter_read`: the owner's record.
            call EnterRead();
JR_younger:
            \* The same pass polls the younger: its owner has a record now, so it leaves the queue,
            \* gives back the share it was granted, and reads again on that record. MUTATION
            \* queued_read_claims_again: a granted read records itself as a first read.
            if queuedJ /\ Mutation = "queued_read_claims_again" then
                call EnterRead();                   \* a second record for the same owner
                goto O_next;
            else
                with x = CHOOSE x \in Find(self) : TRUE do
                    recReads[x] := recReads[x] + 1; \* one more read on the owner's record
                end with;
                rguards[self] := rguards[self] + 1; \* and its guard
                if queuedJ then
                    rawReaders := rawReaders - 1;   \* the granted share goes back
                    granted := granted - 1;         \* nothing granted is left over
                end if;
                goto O_next;
            end if;
        elsif st = "WQR" then
WQ_look:
            \* The write's first poll: taken at once if nobody holds the lock; else it queues.
            if ~rawWriter /\ rawReaders = 0 then
                rawWriter := TRUE;                  \* the plain lock's write, at once
                wshare[self] := TRUE;               \* the owner's
                writeDepth := 1;                    \* one level
                wguards[self] := wguards[self] + 1; \* the write guard
                goto WQ_storeNow;
            else
                goto WQ_read;
            end if;
WQ_storeNow:
            \* Taken at once: the writer id, then the read inside the write ...
            writerId := self;                       \* the owner writes
            call EnterRead();                       \* a writer may also read: its record
WQ_letGo:
            \* ... and the write goes, downgraded to the owner's read (`write_unlock`).
            writeDepth := 0;                        \* the last write level
            wguards[self] := wguards[self] - 1;     \* its guard goes
            writerId := NoOwner;                    \* the writer id is cleared
WQ_down:
            \* The owner still reads: the write becomes its read.
            rawWriter := FALSE;                     \* no writer
            rawReaders := rawReaders + 1;           \* one more reader
            wshare[self] := FALSE;                  \* the owner's write is gone
            rshare[self] := TRUE;                   \* its read share is here
            downgraded := TRUE;                     \* (record) a downgrade
            goto O_next;
WQ_read:
            \* Queued, the write waits; meanwhile the owner reads, past a writer not owed yet:
            \* a share of the plain lock, then its record.
            await ~rawWriter;                       \* no writer holds it
            rawReaders := rawReaders + 1;           \* the owner's share
            rshare[self] := TRUE;                   \* the owner's
            call EnterRead();                       \* and its record
WQ_repoll:
            \* The write is polled again: its owner reads without writing, so it leaves the queue
            \* (the owner's share kept a grant out) and is refused. MUTATION
            \* queued_write_not_refused: a queued write only looks at its node, and waits for the
            \* owner's own read.
            if Mutation = "queued_write_not_refused" then
                goto WQ_wait;
            else
                refused[self] := TRUE;              \* `ReadHeldByOwner`
                goto O_next;
            end if;
WQ_wait:
            \* The write waits for every read to go, the owner's own among them.
            await ~rawWriter /\ rawReaders = 0;   \* never, while the owner reads
            goto O_next;                            \* (unreachable)
        elsif st = "R" then
O_findR:
            \* `reenter_read`: its own record, or its own write.
            if Find(self) # {} /\ Mutation # "read_ignores_own_record" then
                with x = CHOOSE x \in Find(self) : TRUE do
                    recReads[x] := recReads[x] + 1;
                end with;
                rguards[self] := rguards[self] + 1;
                goto O_next;
            elsif writerId = self
                  \/ (writerId # NoOwner /\ Mutation = "read_beside_any_writer") then
                call EnterRead();
                goto O_next;
            end if;
O_rawRead:
            await ~rawWriter;
            rawReaders := rawReaders + 1;
            rshare[self] := TRUE;
            call EnterRead();
        elsif st = "W" then
O_findW:
            if writerId = self then
                writeDepth := writeDepth + 1;
                wguards[self] := wguards[self] + 1;
                goto O_next;
            elsif Find(self) # {} /\ Mutation # "write_waits_for_own_read" then
                \* Reading without writing: refused, it would wait for its own read.
                refused[self] := TRUE;
                goto O_skipW;
            end if;
O_rawWrite:
            await ~rawWriter /\ rawReaders = 0;
            rawWriter := TRUE;
            wshare[self] := TRUE;
O_enterW:
            writeDepth := 1;
            wguards[self] := wguards[self] + 1;
O_storeW:
            writerId := self;
        elsif st = "r" then
O_readUnlock:
            with x = CHOOSE x \in Find(self) : TRUE do
                rguards[self] := rguards[self] - 1;
                recReads[x] := recReads[x] - 1;
                if recReads[x] = 0 then
                    recOwner[x] := NoOwner;
                end if;
            end with;
O_readShare:
            \* The last read: its share back, unless the owner writes.
            if Find(self) = {} /\ (writerId # self \/ Mutation = "read_unlock_while_writing") then
                rawReaders := rawReaders - 1;
                rshare[self] := FALSE;
            end if;
        else
O_writeUnlock:
            writeDepth := writeDepth - 1;
            wguards[self] := wguards[self] - 1;
            if writeDepth = 0 then
O_clearW:
                if Mutation # "keep_writer_id" then
                    writerId := NoOwner;
                end if;
O_downOrFree:
                if Find(self) # {} /\ Mutation # "write_unlock_forgets_reads" then
                    \* Still reading: the write becomes its read (`downgrade`).
                    rawWriter := FALSE;
                    rawReaders := rawReaders + 1;
                    wshare[self] := FALSE;
                    rshare[self] := TRUE;
                    downgraded := TRUE;
                else
                    rawWriter := FALSE;
                    wshare[self] := FALSE;
                end if;
            end if;
        end if;
O_next:
        i := i + 1;
    end while;
O_end:
    goto Done;
O_skipW:
    \* The refused write's own release, if the program has one, is skipped too.
    i := IF i + 1 <= Len(Progs[self]) /\ Progs[self][i + 1] = "w" THEN i + 2 ELSE i + 1;
    goto O_step;
end process;
end algorithm; *)
=============================================================================

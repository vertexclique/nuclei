----------------------------- MODULE LentFile -----------------------------
(***************************************************************************)
(* A file handle's reads, writes, syncs and seeks (src/fs/file/mod.rs:    *)
(* `read_owned`, `write_owned`, `fill`, `write`, `write_direct`,           *)
(* `poll_writes`, `poll_orphans`, `counts`, `seek`; src/fs/file/           *)
(* positional.rs: `read_at`, `write_at`, `poll_ordered`, `Out`, `orphan`;  *)
(* src/fs/file/orphans.rs: `Orphans`; src/fs/file/durable.rs: `sync`;     *)
(* src/fs/file/work.rs: `bounce`, `pwrite_owned`), on the file's jobs,    *)
(* made on the blocking pool or on the ring of the task's shard            *)
(* (src/fs/ring.rs). A vectored write at the cursor (src/fs/mod.rs:        *)
(* `write_vectored_owned`) is the lent write of scenario "cursor", its     *)
(* buffers written in turn; a vectored read is a lent read into the first *)
(* buffer with room. A change of a range of the file is a positional       *)
(* write in this model: src/fs/file/space.rs `allocate`, and the write     *)
(* side of src/fs/file/copy.rs `copy_range_to` (one chunk is one           *)
(* positional write, left to the file as an orphan if its future goes),   *)
(* both through `Out` and `poll_ordered`, and `set_len`'s move of the      *)
(* generation (src/fs/file_ops.rs), are covered by scenario "positional";  *)
(* `sync_range` (durable.rs `sync` with a range) is a sync with another    *)
(* call, covered by scenario "sync".                                       *)
(*                                                                          *)
(* In plain words: the file has a cursor and an epoch, which every write   *)
(* and seek at the cursor moves on. Jobs run on the blocking pool or the  *)
(* ring, in any order. A write on the ring may land in two parts, the     *)
(* kernel taking only some of its bytes first: the rest goes out next,    *)
(* and the write is done only once all of it landed. The shard may go     *)
(* while its ring has calls out: each is made on the pool instead, from   *)
(* where it stopped.                                                       *)
(*                                                                          *)
(* Scenario "cursor" (the first one checked): two tasks each read one cell *)
(* at the cursor into their own buffers: a read waits for the write in     *)
(* flight, then its job reads at the cursor, and its result counts only if *)
(* the cursor and the epoch are still those it read at; else it reads      *)
(* again. A third task writes one cell from the pool (a copied write),    *)
(* then one from its own buffer, which waits for the write in flight and  *)
(* is then the write in flight itself, then seeks back to the start (a    *)
(* seek waits for the write in flight). Any task may go away while its job *)
(* is out: a read job is cancelled or its result dropped, and a lent write *)
(* still lands. A file opened to append makes its writes on the pool at   *)
(* its end, wherever the cursor was; once one landed the cursor is the end *)
(* of it, and the writer appends once more after its seek to the start.   *)
(*                                                                          *)
(* Scenario "positional": a task writes cell 0 at an offset of its own     *)
(* (`write_at`), another reads it (`read_at`); neither reads nor moves the *)
(* cursor or the epoch. Each waits first for the cursor writes accepted    *)
(* before it, so the write is not lost under one of those and the read    *)
(* does not read from before one. The cursor writer writes cell 0 once,   *)
(* before, while or after the positional write is out, and flushes. A     *)
(* positional write whose task went away still lands, and every write     *)
(* after that (the cursor writer's, then) waits for it.                    *)
(*                                                                          *)
(* Scenario "generation": a cursor reader reads two cells through the      *)
(* read-ahead (a job reads ahead two cells), and another one cell into its *)
(* own buffer, while a task writes cell 1 at an offset of its own. When a  *)
(* positional write's task takes its end, the file's generation moves on, *)
(* and a cursor read whose job or read-ahead is of an older generation is  *)
(* not counted.                                                            *)
(*                                                                          *)
(* Scenario "sync": the cursor writer's second copied write waits in the   *)
(* write-behind while the first is in flight; a sync task syncs twice once *)
(* both were accepted. A sync waits until the                              *)
(* write-behind went out and every write in flight landed, then its job    *)
(* makes what the file holds durable, or fails; a failed sync makes every  *)
(* later one fail.                                                         *)
(*                                                                          *)
(* Scenario "direct": the file is opened for direct IO: a read job reads   *)
(* exactly the cell asked for (no read-ahead), and a copied write is one   *)
(* kernel write of exactly its bytes, which waits for the write in flight  *)
(* instead of joining the write-behind.                                    *)
(*                                                                          *)
(* Every call runs on the handle's own descriptor (src/fs/file/mod.rs,     *)
(* `FileIo::new`; src/io/source/requests.rs, `Lease`): no copy of it is    *)
(* made, and every job holds a lease on the handle's object until it ran. *)
(* Once no task borrows the handle it ends (`Ending`): dropped, it hands   *)
(* its object over and the last lease out closes it; handed back by       *)
(* `into_inner`, it waits until no lease is out, and the caller may close  *)
(* the object at once. No call ever runs on a descriptor that closed.      *)
(*                                                                          *)
(* What is abstract: the file state's guard (a step under it runs whole), *)
(* how much a job reads or writes (a cell, or the cells asked for), and    *)
(* where in time a positional call's own future runs: its end is taken by *)
(* its task in one step.                                                   *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    MayLeave,    \* whether a task may go away while its job is out
    Appending,   \* whether the file was opened to append
    Scenario,    \* which tasks run: "cursor", "positional", "generation", "sync" or "direct"
    SyncMayFail, \* whether the device may fail a sync
    Ending,      \* how the handle ends once no task borrows it: "none" (it outlives the run), "drop", "into_inner"
    Mutation     \* "none", or the name of the one rule this run takes away

\* The cells of the file, numbered from its start.
Cells == 0..5
\* No job: an `at` no real job has.
NoJob == [at |-> -1]
\* No write-behind.
NoBehind == [at |-> -1]
\* Nothing read ahead.
NoAhead == [at |-> -1]
\* No value recorded.
None == -99
\* The tasks that read one cell at the cursor into their own buffers.
Readers == CASE Scenario = "cursor" -> {"r1", "r2"} [] Scenario = "generation" -> {"r1"} [] OTHER -> {}
\* The task that reads at the cursor through the read-ahead, and how many cells it reads.
BReaders == IF Scenario \in {"generation", "direct"} THEN {"dr"} ELSE {}
BReads == IF Scenario = "generation" THEN 2 ELSE 1
\* The task that writes at offsets of its own.
PWriters == IF Scenario \in {"positional", "generation"} THEN {"pw"} ELSE {}
\* The task that reads at an offset of its own.
PReaders == IF Scenario = "positional" THEN {"pr"} ELSE {}
\* The task that syncs.
Syncers == IF Scenario = "sync" THEN {"sy"} ELSE {}
\* Whether the file is opened for direct IO.
Direct == Scenario = "direct"
\* The cells the positional writer writes, one after the other, and their values.
PCells == IF Scenario = "generation" THEN <<1>> ELSE <<0>>
PVals == IF Scenario = "generation" THEN <<7>> ELSE <<4>>
\* The values of the cursor writer's copied writes after its first one, each accepted at once.
Behinds == CASE Scenario = "direct" -> <<6, 8>> [] Scenario = "sync" -> <<6>> [] OTHER -> <<>>
\* The cells a buffered read job reads at least (the read-ahead).
ReadAhead == 2
\* Every task (the pool, which makes the jobs' calls, is not one).
Tasks == Readers \cup BReaders \cup PWriters \cup PReaders \cup Syncers \cup {"writer"}

(* --algorithm LentFile
variables
    holder = "none",                   \* who holds the file state's guard
    file = [i \in Cells |-> 0],        \* what the file holds: 0 before any write
    size = 0,                          \* the file's end: one past the last cell written
    cursor = 0,                        \* where the next cursor read or write starts
    epoch = 0,                         \* moves on with every cursor write and seek
    writing = NoJob,                   \* the cursor write in flight: [at, vals, lent, ran, on, part]
    behind = NoBehind,                 \* the write-behind: [at, vals], accepted after the one in flight
    lent = "none",                     \* the lent write: "none", "out", "done" or "orphan"
    jobs = [r \in Readers |-> NoJob],  \* each reader's job: [at, epoch, gen, ran, val, on]
    shut = FALSE,                      \* the shard went: its ring makes no more calls
    written = {},                      \* the cursor writes accepted: <<cell, value>>
    taken = {},                        \* the cursor reads that counted: <<reader, cell, value>>
    pgen = 0,                          \* the file's generation: positional writes whose end was taken
    pjob = NoJob,                      \* the positional write out: [at, val, k, ran, on, part]
    porphan = "none",                  \* its task: "none", "out" (it waits) or "orphan" (it went away)
    preturned = {},                    \* (record) the positional writes that returned to their task
    pleft = {},                        \* (record) the positional writes their task left
    beforeP = [c \in Cells |-> FALSE], \* (record) a cursor write to the cell came before the positional one
    duringP = [c \in Cells |-> FALSE], \* (record) a cursor write to the cell came while the positional one was out
    afterP = [c \in Cells |-> None],   \* (record) the last cursor write to the cell after the positional one ended
    djob = NoJob,                      \* the read-ahead reader's job: [at, len, epoch, gen, ran, vals, on]
    ahead = NoAhead,                   \* cells read ahead at the cursor: [at, vals, epoch, gen]
    prjob = NoJob,                     \* the positional read's job: [at, ran, val, on]
    prBefore = FALSE,                  \* (record) a cursor write to its cell came before the positional read
    prGot = None,                      \* (record) what the positional read returned
    sjob = NoJob,                      \* the sync's job: [ran, ok, on, drain]
    durable = [c \in Cells |-> 0],     \* what the device holds for sure: the file as the last good sync saw it
    okClaims = {},                     \* (record) the writes a sync that succeeded says are durable
    syncFailed = FALSE,                \* the handle's syncs failed once: every later one fails
    failedOnce = FALSE,                \* (record) a sync returned a failure
    okAfterFail = FALSE,               \* (record) a sync returned success after one returned a failure
    creads = {},                       \* (record) the reads callers asked for: <<cell, cells>>
    kreads = {},                       \* (record) the reads the kernel was asked for
    cwrites = {},                      \* (record) the writes callers made
    kwrites = {},                      \* (record) the writes the kernel was asked for
    stale = FALSE,                     \* (record) a read counted a value the cell no longer holds
    torn = FALSE,                      \* (record) a read counted a cell a write had landed only part of
    redone = FALSE,                    \* (record) a read found the cursor or epoch moved and read again
    posRedone = FALSE,                 \* (record) a read read again only because a positional write ended
    orphanLanded = FALSE,              \* (record) the lent write of a task that went away landed
    posOrphanLanded = FALSE,           \* (record) the positional write of a task that went away landed
    short = FALSE,                     \* (record) a write on the ring landed in two parts
    movedMid = FALSE,                  \* (record) a write cut short on the ring was finished on the pool
    overwrote = FALSE,                 \* (record) an append landed on a cell already written
    offEnd = FALSE,                    \* (record) an append landed and the cursor is not the end
    pastCursor = FALSE,                \* (record) an append landed elsewhere than the cursor it was made at
    objOpen = TRUE,                    \* the handle's object, its descriptor, is open
    gone = FALSE,                      \* the handle ended (dropped, or its object handed back)
    usedClosed = FALSE;                \* (record) a job's call ran on a descriptor that closed

define
    \* A cursor write is in flight: submitted and not landed.
    InFlight == writing # NoJob /\ ~writing.ran
    \* A positional write whose task went away has not been taken by the file yet.
    OrphanOut == porphan = "orphan"
    \* Something a later write or read must wait for: the cursor write in flight, the write-behind,
    \* or a positional write whose task went away.
    Pending == InFlight \/ behind # NoBehind \/ OrphanOut
    \* What is still pending once `poll_writes` ran, said before it runs: a write in flight stays in
    \* flight, the write-behind goes in flight (or waits), and a positional write left by its task
    \* stays until it landed.
    StillPending == InFlight \/ behind # NoBehind \/ (IF OrphanOut THEN ~pjob.ran ELSE FALSE)
    \* What a task waiting for `Pending` to clear waits for: the write in flight landed, and a
    \* positional write whose task went away landed (a holder then takes them).
    Progress == ~InFlight /\ (IF OrphanOut THEN pjob.ran ELSE TRUE)
    \* Where a new job goes: the ring of the task's shard while it runs one, or the pool.
    Wheres == IF shut THEN {"pool"} ELSE {"ring", "pool"}
    \* Where a cursor write goes: an append always on the pool.
    WriteWheres == IF Appending THEN {"pool"} ELSE Wheres
    \* A job its runner may make now: the pool's always, the ring's while the shard runs.
    Runs(j) == j.on = "pool" \/ ~shut
    \* Every task is done.
    AllDone == \A p \in Tasks : pc[p] = "Done"
    \* The positional writes that ended for their task: returned, or left when it went away.
    PEnded == preturned \cup pleft
    \* A positional write is out: submitted and its end not taken.
    PosOut == pjob # NoJob
    \* What the file holds at cell `c` (nothing past its cells).
    FileAt(c) == IF c \in Cells THEN file[c] ELSE 0
    \* The cell is one the positional writer writes, so more than one write reaches it.
    Shared(c) == PWriters # {} /\ \E j \in 1..Len(PCells) : PCells[j] = c
    \* A job's result counts: the cursor, the epoch and the generation are the ones it read at.
    \* MUTATION no_epoch_check: a result counts if only the cursor is the same.
    \* MUTATION no_gen_check: a result counts whatever positional write ended meanwhile.
    Counts(j) == /\ j.at = cursor
                 /\ (j.epoch = epoch \/ Mutation = "no_epoch_check")
                 /\ (j.gen = pgen \/ Mutation = "no_gen_check")
    \* The leases out on the handle's descriptor: every call submitted and not run yet holds one
    \* (the cursor write in flight, each read job, the positional calls, the sync).
    Leases == (IF InFlight THEN 1 ELSE 0)
              + Cardinality({r \in Readers : jobs[r] # NoJob /\ ~jobs[r].ran})
              + (IF djob # NoJob /\ ~djob.ran THEN 1 ELSE 0)
              + (IF PosOut /\ ~pjob.ran THEN 1 ELSE 0)
              + (IF prjob # NoJob /\ ~prjob.ran THEN 1 ELSE 0)
              + (IF sjob # NoJob /\ ~sjob.ran THEN 1 ELSE 0)
end define;

\* `poll_writes`, under the guard: a cursor write that ran is taken out of flight (the lent one's
\* buffer goes back to its task, or with the job if the task went away); a positional write whose
\* task went away is taken once it landed, and the generation moves on; then the write-behind goes
\* out, once nothing is ahead of it.
\* `seekTo` is where a seek in the same step puts the cursor after, or -1.
macro PollWrites(seekTo) begin
    if writing # NoJob /\ writing.ran then
        if writing.lent then
            if lent = "orphan" then orphanLanded := TRUE; end if;
            lent := IF lent = "out" THEN "done" ELSE "none";
        end if;
    end if;
    \* An append leaves the cursor at the end of what it wrote.
    \* MUTATION cursor_stays: the cursor stays where the append was made.
    cursor := IF seekTo >= 0 THEN seekTo
              ELSE IF writing # NoJob /\ writing.ran /\ Appending /\ Mutation # "cursor_stays"
                   THEN writing.at + 1
                   ELSE cursor;
    if seekTo < 0 /\ writing # NoJob /\ writing.ran /\ Appending /\ cursor # size then
        offEnd := TRUE;
    end if;
    \* A positional write left by its task: once it landed, the file takes it and the generation moves on.
    \* MUTATION orphan_no_gen: the file takes it with the generation where it was.
    if OrphanOut /\ pjob.ran then
        posOrphanLanded := TRUE;
        pgen := IF Mutation = "orphan_no_gen" THEN pgen ELSE pgen + 1;
        pjob := NoJob;
        porphan := "none";
    end if;
    \* The write-behind goes out once no write is in flight and no positional write left by its
    \* task is out; it goes where the shard's calls go.
    \* MUTATION orphan_unordered: it goes out while a positional write left by its task is out.
    with free = (writing = NoJob \/ writing.ran),
         go = /\ behind # NoBehind
              /\ (writing = NoJob \/ writing.ran)
              /\ (porphan # "orphan" \/ Mutation = "orphan_unordered") do
        writing := IF go THEN [at |-> behind.at, vals |-> behind.vals, lent |-> FALSE, ran |-> FALSE,
                               on |-> IF shut \/ Appending THEN "pool" ELSE "ring", part |-> 0]
                   ELSE IF free THEN NoJob ELSE writing;
        kwrites := IF go THEN kwrites \cup {<<behind.at, Len(behind.vals)>>} ELSE kwrites;
        behind := IF go THEN NoBehind ELSE behind;
    end with;
end macro;

\* What the records say of a cursor write of `v` to cell `c`: after a positional write to the
\* cell ended, it is the one that must be in the file; while one is out, the two race.
macro PosHistory(c, v) begin
    if \E j \in PEnded : PCells[j] = c then
        afterP[c] := v;
    elsif PosOut /\ pjob.at = c then
        duringP[c] := TRUE;
    end if;
end macro;

\* `write`, under the guard (just after `poll_writes`): a copied write of `v` at the cursor. With
\* nothing ahead of it, it goes in flight at once; else it joins the write-behind, after the bytes
\* there, and goes out with them as one kernel write.
macro AcceptCopy(v) begin
    if Pending /\ (Mutation # "orphan_unordered" \/ InFlight \/ behind # NoBehind) then
        behind := IF behind = NoBehind THEN [at |-> cursor, vals |-> <<v>>]
                  ELSE [behind EXCEPT !.vals = Append(@, v)];
    else
        with where \in WriteWheres do
            writing := [at |-> cursor, vals |-> <<v>>, lent |-> FALSE, ran |-> FALSE, on |-> where, part |-> 0];
        end with;
        kwrites := kwrites \cup {<<cursor, 1>>};
    end if;
    cwrites := cwrites \cup {<<cursor, 1>>};
    written := written \cup {<<IF Appending THEN -1 ELSE cursor, v>>};
    PosHistory(cursor, v);
    cursor := cursor + 1;
    epoch := epoch + 1;
end macro;

\* The blocking pool and the shard's ring, which make the jobs' calls; and the shard's going.
fair process pool = "pool"
begin
P_loop:
    while ~(/\ AllDone /\ ~InFlight
            /\ \A r \in Readers : jobs[r] = NoJob \/ jobs[r].ran
            /\ (djob = NoJob \/ djob.ran) /\ (pjob = NoJob \/ pjob.ran)
            /\ (prjob = NoJob \/ prjob.ran) /\ (sjob = NoJob \/ sjob.ran)) do
        either
            await InFlight /\ Runs(writing);
            either
                \* All of the write, or the rest of one cut short; an append at the end.
                \* MUTATION append_at_cursor: an append lands where the cursor was.
                with cell = IF Appending /\ Mutation # "append_at_cursor" THEN size ELSE writing.at,
                     len = Len(writing.vals) do
                    \* A call on a descriptor that closed is recorded.
                    if ~objOpen then usedClosed := TRUE; end if;
                    if Appending /\ file[cell] # 0 then overwrote := TRUE; end if;
                    if Appending /\ cell # writing.at then pastCursor := TRUE; end if;
                    \* Each cell of the write gets its value.
                    file := [c \in Cells |-> IF c >= cell /\ c < cell + len THEN writing.vals[c - cell + 1]
                                             ELSE file[c]];
                    writing := [writing EXCEPT !.at = cell, !.ran = TRUE];
                    size := IF cell + len > size THEN cell + len ELSE size;
                end with;
            or
                \* The ring's call takes only part of a one-cell write: half the cell holds the value.
                await writing.on = "ring" /\ writing.part = 0 /\ Len(writing.vals) = 1;
                if ~objOpen then usedClosed := TRUE; end if;
                file[writing.at] := -writing.vals[1];
                short := TRUE;
                \* MUTATION no_continuation: a write cut short counts as done.
                if Mutation = "no_continuation" then
                    writing.ran := TRUE;
                else
                    writing.part := 1;
                end if;
            end either;
        or
            \* A reader's job reads its cell.
            with r \in {q \in Readers : jobs[q] # NoJob /\ ~jobs[q].ran /\ Runs(jobs[q])} do
                if ~objOpen then usedClosed := TRUE; end if;
                jobs[r].val := file[jobs[r].at] || jobs[r].ran := TRUE;
            end with;
        or
            \* The read-ahead reader's job reads its cells (past the file's cells, nothing).
            await djob # NoJob /\ ~djob.ran /\ Runs(djob);
            if ~objOpen then usedClosed := TRUE; end if;
            djob.vals := [i \in 1..djob.len |-> FileAt(djob.at + i - 1)]
                || djob.ran := TRUE;
        or
            \* The positional read's job reads its cell.
            await prjob # NoJob /\ ~prjob.ran /\ Runs(prjob);
            if ~objOpen then usedClosed := TRUE; end if;
            prjob.val := file[prjob.at] || prjob.ran := TRUE;
        or
            \* The positional write lands whole, or the ring's call takes half of it first.
            await PosOut /\ ~pjob.ran /\ Runs(pjob);
            if ~objOpen then usedClosed := TRUE; end if;
            either
                file[pjob.at] := pjob.val;
                pjob.ran := TRUE;
            or
                await pjob.on = "ring" /\ pjob.part = 0;
                file[pjob.at] := -pjob.val;
                pjob.part := 1;
            end either;
        or
            \* The sync's job: the device keeps what the file holds now, or the call fails. A sync
            \* put on the ring with a drain (the mutation) starts only once the ring's writes ended.
            await sjob # NoJob /\ ~sjob.ran /\ Runs(sjob);
            await sjob.drain /\ sjob.on = "ring" => ~(InFlight /\ writing.on = "ring");
            if ~objOpen then usedClosed := TRUE; end if;
            with ok \in IF SyncMayFail THEN {TRUE, FALSE} ELSE {TRUE} do
                durable := IF ok THEN file ELSE durable;
                sjob.ok := ok || sjob.ran := TRUE;
            end with;
        or
            \* The shard goes: every call its ring has out is made on the pool.
            await ~shut;
            shut := TRUE;
            jobs := [r \in Readers |->
                        IF jobs[r] # NoJob /\ ~jobs[r].ran THEN [jobs[r] EXCEPT !.on = "pool"]
                        ELSE jobs[r]];
            djob := IF djob # NoJob /\ ~djob.ran THEN [djob EXCEPT !.on = "pool"] ELSE djob;
            prjob := IF prjob # NoJob /\ ~prjob.ran THEN [prjob EXCEPT !.on = "pool"] ELSE prjob;
            pjob := IF PosOut /\ ~pjob.ran THEN [pjob EXCEPT !.on = "pool"] ELSE pjob;
            sjob := IF sjob # NoJob /\ ~sjob.ran THEN [sjob EXCEPT !.on = "pool"] ELSE sjob;
            if InFlight /\ writing.on = "ring" then
                \* MUTATION cancel_ends: a write cancelled as its shard went ends there.
                if Mutation = "cancel_ends" then
                    writing.ran := TRUE;
                else
                    if writing.part = 1 then movedMid := TRUE; end if;
                    writing.on := "pool";
                end if;
            end if;
        end either;
    end while;
end process;

\* Readers of one cell at the cursor into their own buffers (`read_owned`).
fair process reader \in Readers
begin
R_hold:
    await holder = "none";
    holder := self;
R_submit:
    \* A read sees the writes before it: it waits for the one in flight.
    PollWrites(-1);
    if StillPending then
        holder := "none";
R_wait_writes:
        await Progress;
        goto R_hold;
    else
        with where \in Wheres do
            jobs[self] := [at |-> cursor, epoch |-> epoch, gen |-> pgen, ran |-> FALSE, val |-> 0, on |-> where];
        end with;
        holder := "none";
    end if;
R_wait:
    either
        await jobs[self].ran;
    or
        \* The task goes away: its job is cancelled, or its result dropped.
        await MayLeave;
        jobs[self] := NoJob;
        goto Done;
    end either;
R_rehold:
    await holder = "none";
    holder := self;
R_take:
    if Counts(jobs[self]) then
        \* A stale or torn value counts only while a positional write races the read.
        if jobs[self].val # file[jobs[self].at] /\ ~PosOut then stale := TRUE; end if;
        if jobs[self].val < 0 /\ ~PosOut then torn := TRUE; end if;
        taken := taken \cup {<<self, jobs[self].at, jobs[self].val>>};
        cursor := cursor + 1;
        jobs[self] := NoJob;
        holder := "none";
        goto Done;
    else
        redone := TRUE;
        jobs[self] := NoJob;
        holder := "none";
        goto R_hold;
    end if;
end process;

\* A reader of `BReads` cells at the cursor through the read-ahead (`poll_read`, `fill`).
fair process breader \in BReaders
variables got = 0;   \* the cells it has taken
begin
D_hold:
    await holder = "none";
    holder := self;
    PollWrites(-1);
D_fill:
    if Pending then
        \* A read sees the writes before it: it waits for them.
        holder := "none";
D_wait_writes:
        await Progress;
        goto D_hold;
    elsif /\ ahead # NoAhead /\ ahead.at = cursor /\ ahead.epoch = epoch
          /\ (ahead.gen = pgen \/ Mutation = "ahead_no_gen") then
        \* Cells read ahead at the cursor, of the file as it is: the read takes the first.
        \* MUTATION ahead_no_gen: cells read ahead before a positional write ended still count.
        if ahead.vals[1] # FileAt(cursor) /\ ~PosOut then stale := TRUE; end if;
        if ahead.vals[1] < 0 /\ ~PosOut then torn := TRUE; end if;
        taken := taken \cup {<<self, cursor, ahead.vals[1]>>};
        cursor := cursor + 1;
        ahead := IF Len(ahead.vals) > 1 THEN [ahead EXCEPT !.at = @ + 1, !.vals = Tail(@)] ELSE NoAhead;
        got := got + 1;
        holder := "none";
D_next:
        if got < BReads then
            goto D_hold;
        else
            goto Done;
        end if;
    else
        \* A job reads at the cursor: exactly the cell asked for on a direct file, else the read-ahead.
        \* MUTATION direct_through_ahead: a direct file's read reads ahead too.
        with where \in Wheres, len = IF Direct /\ Mutation # "direct_through_ahead" THEN 1 ELSE ReadAhead do
            djob := [at |-> cursor, len |-> len, epoch |-> epoch, gen |-> pgen, ran |-> FALSE,
                     vals |-> <<>>, on |-> where];
            kreads := kreads \cup {<<cursor, len>>};
        end with;
        creads := creads \cup {<<cursor, 1>>};
        ahead := NoAhead;
        holder := "none";
D_wait:
        await djob.ran;
D_rehold:
        await holder = "none";
        holder := self;
D_take:
        \* Its cells become the read-ahead if they are of the file as it is; else it reads again.
        if Counts(djob) then
            ahead := [at |-> djob.at, vals |-> djob.vals, epoch |-> djob.epoch, gen |-> djob.gen];
        else
            redone := TRUE;
            if djob.at = cursor /\ djob.epoch = epoch then posRedone := TRUE; end if;
        end if;
        djob := NoJob;
        holder := "none";
        goto D_hold;
    end if;
end process;

\* A writer of cells at offsets of its own (`write_at`); it never touches the cursor.
fair process pwriter \in PWriters
variables k = 1;   \* which of its writes it makes now
begin
PW_loop:
    while k <= Len(PCells) do
PW_order:
        await holder = "none";
        holder := self;
        PollWrites(-1);
PW_go:
        \* It waits for the cursor writes accepted before it, and a positional write left by its task.
        \* MUTATION pos_skips_wait: it goes while a cursor write accepted before it is out.
        if Pending /\ Mutation # "pos_skips_wait" then
            holder := "none";
PW_wait_writes:
            await Progress;
            goto PW_order;
        else
            with where \in Wheres do
                pjob := [at |-> PCells[k], val |-> PVals[k], k |-> k, ran |-> FALSE, on |-> where, part |-> 0];
            end with;
            beforeP[PCells[k]] := \E w \in written : w[1] = PCells[k];
            \* MUTATION pos_bumps_epoch: the positional write moves the epoch on.
            epoch := IF Mutation = "pos_bumps_epoch" THEN epoch + 1 ELSE epoch;
            porphan := "out";
            cwrites := cwrites \cup {<<PCells[k], 1>>};
            kwrites := kwrites \cup {<<PCells[k], 1>>};
            holder := "none";
        end if;
PW_wait:
        either
            \* Its task takes the end: the generation moves on.
            await pjob.ran;
            pgen := pgen + 1;
            preturned := preturned \cup {k};
            pjob := NoJob;
            porphan := "none";
        or
            \* Its task goes away while the write is out: the write is left to the file.
            await MayLeave /\ ~pjob.ran;
            porphan := "orphan";
            pleft := pleft \cup {k};
            goto Done;
        end either;
PW_next:
        k := k + 1;
    end while;
end process;

\* A reader of cell 0 at an offset of its own (`read_at`); it never touches the cursor.
fair process preader \in PReaders
begin
PR_order:
    await holder = "none";
    holder := self;
    prBefore := \E w \in written : w[1] = 0;
    PollWrites(-1);
PR_go:
    \* It waits for the cursor writes accepted before it.
    \* MUTATION pr_skips_wait: it reads while a cursor write accepted before it is out.
    if Pending /\ Mutation # "pr_skips_wait" then
        holder := "none";
PR_wait_writes:
        await Progress;
        goto PR_order;
    else
        with where \in Wheres do
            prjob := [at |-> 0, ran |-> FALSE, val |-> 0, on |-> where];
        end with;
        creads := creads \cup {<<0, 1>>};
        kreads := kreads \cup {<<0, 1>>};
        holder := "none";
    end if;
PR_wait:
    await prjob.ran;
    prGot := prjob.val;
    prjob := NoJob;
end process;

\* A task that syncs twice (`sync_all`, `sync_data`), once the cursor writer's writes it covers
\* were accepted.
fair process syncer \in Syncers
variables round = 1,     \* which of its two syncs it makes
          claimed = {},  \* the writes this sync must make durable
          refused = FALSE; \* the sync failed without a call: one before it failed
begin
S_call:
    \* Called once the writer accepted its copied writes: what was accepted, and the positional
    \* writes that returned, are what it covers.
    await pc["writer"] \notin {"W_copy", "W_b_hold", "W_b_accept", "W_b_wait"};
    claimed := written \cup {<<PCells[j], PVals[j]>> : j \in preturned};
S_hold:
    await holder = "none";
    holder := self;
    PollWrites(-1);
S_go:
    if syncFailed then
        \* The handle's syncs failed before: this one fails too, with no call.
        refused := TRUE;
        holder := "none";
        goto S_end;
    elsif \/ ~Pending
          \* MUTATION sync_no_flush: the sync goes with writes still behind or in flight.
          \/ Mutation = "sync_no_flush"
          \* MUTATION sync_drain: the sync goes once the write-behind went out, its order left to
          \* the ring's drain.
          \/ (Mutation = "sync_drain" /\ behind = NoBehind) then
        with where \in Wheres do
            sjob := [ran |-> FALSE, ok |-> TRUE, on |-> where, drain |-> Mutation = "sync_drain"];
        end with;
        holder := "none";
    else
        \* It waits for the writes accepted before it to land.
        holder := "none";
S_wait_writes:
        await Progress;
        goto S_hold;
    end if;
S_wait:
    await sjob.ran;
S_end:
    if refused \/ ~sjob.ok then
        \* The sync fails, and the handle keeps the failure for every later sync.
        \* MUTATION sync_forgets: the failure is forgotten once reported.
        syncFailed := Mutation # "sync_forgets";
        failedOnce := TRUE;
    else
        okClaims := okClaims \cup claimed;
        if failedOnce then okAfterFail := TRUE; end if;
    end if;
    sjob := NoJob;
    refused := FALSE;
    round := round + 1;
S_again:
    if round <= 2 then
        goto S_call;
    end if;
end process;

\* The cursor writer: copied writes, a lent write, a seek; then an append or a second write of cell 0.
\* What each scenario needs of it: "cursor" all but the second write of cell 0; "positional" the first
\* write, the seek and the second write of cell 0; "sync" and "direct" the copied writes and a flush;
\* "generation" nothing.
fair process writer = "writer"
variables b = 1;   \* which of its copied writes after the first it makes next
begin
W_copy:
    \* A copied write: nothing is in flight, so it goes at once (or behind a positional write left
    \* by its task). The readers of the "generation" scenario have no writer beside them.
    await holder = "none" \/ Scenario = "generation";
    if Scenario = "generation" then
        goto Done;
    else
        AcceptCopy(1);
        if Len(Behinds) = 0 then
            goto W_hold;
        end if;
    end if;
W_b_hold:
    await holder = "none";
    holder := "writer";
    PollWrites(-1);
W_b_accept:
    \* A direct write goes alone: it waits for the one in flight, with nothing behind it.
    \* MUTATION direct_through_behind: a direct file's write joins the write-behind.
    if Direct /\ Mutation # "direct_through_behind" /\ Pending then
        holder := "none";
W_b_wait:
        await Progress;
        goto W_b_hold;
    else
        AcceptCopy(Behinds[b]);
        b := b + 1;
        holder := "none";
        if b <= Len(Behinds) then
            goto W_b_hold;
        end if;
    end if;
W_hold:
    \* The lent write and the seek are the "cursor" scenario's; the others flush.
    if Scenario # "cursor" then
        goto W_flush;
    else
        await holder = "none";
        holder := "writer";
    end if;
W_poll:
    PollWrites(-1);
W_lend:
    \* MUTATION lent_skips_wait: the lent write goes in flight over the one there.
    if Pending /\ Mutation # "lent_skips_wait" then
        holder := "none";
W_wait_writes:
        await Progress;
        goto W_hold;
    else
        with where \in WriteWheres do
            writing := [at |-> cursor, vals |-> <<2>>, lent |-> TRUE, ran |-> FALSE, on |-> where, part |-> 0];
        end with;
        written := written \cup {<<IF Appending THEN -1 ELSE cursor, 2>>};
        cwrites := cwrites \cup {<<cursor, 1>>};
        kwrites := kwrites \cup {<<cursor, 1>>};
        PosHistory(cursor, 2);
        cursor := cursor + 1;
        epoch := epoch + 1;
            lent := "out";
        holder := "none";
    end if;
W_wait:
    either
        \* Woken when its job ran (the pool wakes the file's tasks).
        await (writing # NoJob /\ writing.ran) \/ lent = "done";
        goto W_take;
    or
        await MayLeave /\ holder = "none";
        \* MUTATION orphan_cancelled: the write of a task that went away is cancelled.
        if Mutation = "orphan_cancelled" /\ InFlight then
            writing := NoJob;
            lent := "none";
        elsif lent = "out" then
            lent := "orphan";
        else
            lent := "none";
        end if;
        goto Done;
    end either;
W_take:
    await holder = "none";
    holder := "writer";
W_poll_again:
    PollWrites(-1);
W_taken:
    if lent = "done" then
        lent := "none";
        holder := "none";
    else
        holder := "none";
        goto W_wait;
    end if;
W_seek:
    \* A seek back to the start waits for the writes accepted before it; the epoch moves on.
    await holder = "none";
    holder := "writer";
W_seeked:
    \* A write in flight stays in flight: the seek waits; else it goes to the start.
    PollWrites(IF StillPending THEN -1 ELSE 0);
    if StillPending then
        holder := "none";
        goto W_seek;
    else
        epoch := epoch + 1;
        holder := "none";
    end if;
W_more:
    if ~Appending then
        goto Done;
    end if;
W_append:
    \* Appending after the seek: nothing is in flight (the seek waited), so it goes at once.
    await holder = "none";
    writing := [at |-> cursor, vals |-> <<3>>, lent |-> FALSE, ran |-> FALSE, on |-> "pool", part |-> 0];
    written := written \cup {<<-1, 3>>};
    cursor := cursor + 1;
    epoch := epoch + 1;
W_append_wait:
    \* A seek after it would wait for it, then take where it left the cursor.
    await holder = "none" /\ ~InFlight;
    PollWrites(-1);
    goto Done;
W_flush:
    await holder = "none";
    holder := "writer";
    PollWrites(-1);
W_flushed:
    if Pending then
        holder := "none";
W_flush_wait:
        await Progress;
        goto W_flush;
    else
        holder := "none";
    end if;
end process;

\* The handle's owner: once no task borrows the handle, it ends as `Ending` says.
fair process owner = "owner"
begin
O_end:
    if Ending = "none" then goto Done; end if;
O_wait:
    await AllDone;
    if Ending = "drop" then
        \* Dropped: the object goes to the source; the last lease out closes it.
        gone := TRUE;
        \* MUTATION close_at_drop: the descriptor closes at the drop, jobs still out on it.
        if Mutation = "close_at_drop" then objOpen := FALSE; end if;
    else
        \* `into_inner`: the writes behind land and every job out ends first (the retire waits
        \* for the leases); the caller then has its object back and may close it at once.
        \* MUTATION into_inner_early: it hands the object back at once.
        await Leases = 0 \/ Mutation = "into_inner_early";
        gone := TRUE;
        objOpen := FALSE;
    end if;
O_close:
    \* The last lease of a dropped handle's object closes it.
    await Leases = 0;
    objOpen := FALSE;
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

\* A read counts only a value its cell still holds when the read is taken, unless a positional
\* write raced it (it was out when the read was taken: the caller's own race, as with pread).
NoStaleRead == ~stale

\* No read counts a cell a write had landed only part of, unless a positional write raced it.
NoTornRead == ~torn

\* Once every task is done and no write is in flight, every cursor write accepted is in the file:
\* at its cell (one no positional write reaches), or anywhere for an append (-1).
AllLanded == (AllDone /\ ~InFlight) =>
    \A w \in written : IF w[1] >= 0 THEN Shared(w[1]) \/ file[w[1]] = w[2]
                       ELSE \E c \in Cells : file[c] = w[2]

\* An append never writes over what the file holds.
NoOverwrite == ~overwrote

\* Once an append landed and was taken, the cursor is the file's end.
CursorAtEnd == ~offEnd

\* Once all is done, each cell a positional write reached holds the write that came last in the
\* order the file keeps: a cursor write accepted after the positional one ended; else the positional
\* one, unless a cursor write raced it (accepted while it was out).
PositionalOrder == (AllDone /\ ~InFlight /\ behind = NoBehind /\ (pjob = NoJob \/ pjob.ran)) =>
    \A j \in PEnded :
        LET c == PCells[j] IN
            IF afterP[c] # None THEN file[c] = afterP[c]
            ELSE duringP[c] \/ file[c] = PVals[j]

\* Positional calls never move the cursor or the epoch: only the cursor's own calls do.
PositionalKeepsCursor ==
    [][\A p \in PWriters \cup PReaders : pc'[p] # pc[p] => (cursor' = cursor /\ epoch' = epoch)]_vars

\* A positional read made after a cursor write to its cell was accepted reads that write or a
\* later one, never what the cell held before it.
PosReadSees == (PReaders # {} /\ pc["pr"] = "Done" /\ prBefore) => prGot # 0

\* Every write a sync that succeeded covers (accepted, or returned for a positional one, before it
\* was called) is durable.
SyncCovers == \A w \in okClaims : w[1] >= 0 => durable[w[1]] = w[2]

\* Once a sync failed, no later sync of the handle says it succeeded.
NoSyncAfterFailure == ~okAfterFail

\* On a direct file every kernel read and write is exactly one a caller made: no read-ahead, and
\* no write that joins others.
DirectExact == Direct => (kreads \subseteq creads /\ kwrites \subseteq cwrites)

\* No job's call ever runs on a descriptor that closed (or that another file took since).
NoCallOnClosed == ~usedClosed

\* `Termination` (every process ends) is the one the translator writes for the fair processes.

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A read finds the cursor or the epoch moved and reads again.
NoRedo == ~redone
\* The write of a task that went away lands.
NoOrphanLanded == ~orphanLanded
\* A read after the seek counts the value the lent write put there.
NoReadOfLent == \A t \in taken : t[3] # 2
\* A write on the ring lands in two parts.
NoShortWrite == ~short
\* A write cut short on the ring, whose shard then went, is finished on the pool.
NoMovedMid == ~movedMid
\* An append lands elsewhere than the cursor it was made at (after the seek to the start).
NoAppendPastCursor == ~pastCursor
\* A read reads again only because a positional write ended while its job was out.
NoPosRedo == ~posRedone
\* A buffered read asks the kernel for more than the caller asked (the read-ahead).
NoReadAhead == kreads \subseteq creads
\* The positional write of a task that went away lands and the file takes it.
NoPosOrphanLanded == ~posOrphanLanded
\* A sync fails.
NoSyncFailure == ~failedOnce
\* A job is still out when the handle was dropped: the job closes its object.
NoJobAfterDrop == ~(gone /\ Leases > 0)
=============================================================================

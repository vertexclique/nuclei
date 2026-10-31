----------------------------- MODULE LentFile -----------------------------
(***************************************************************************)
(* File reads and writes from a task's own buffer (src/fs/file.rs:         *)
(* `read_owned`, `write_owned`, `poll_writes`), on the file's jobs, made   *)
(* on the blocking pool or on the ring of the task's shard (src/fs/ring.rs) *)
(*                                                                          *)
(* In plain words: the file has a cursor and an epoch, which every write   *)
(* and seek moves on. Jobs run on the blocking pool or the ring, in any   *)
(* order. A write on the ring may land in two parts, the kernel taking    *)
(* only some of its bytes first: the rest goes out next, and the write is *)
(* done only once all of it landed. The shard may go while its ring has   *)
(* calls out: each is made on the pool instead, from where it stopped. Two *)
(* tasks each read one cell at the cursor into their own buffers: a read   *)
(* waits for the write in flight, then its job reads at the cursor, and   *)
(* its result counts only if the cursor and the epoch are still those it  *)
(* read at; else it reads again. A third task writes one cell from the     *)
(* pool (a copied write), then one from its own buffer, which waits for   *)
(* the write in flight and is then the write in flight itself, then seeks *)
(* back to the start (a seek waits for the write in flight). Any task may *)
(* go away while its job is out: a read job is cancelled or its result    *)
(* dropped, and a lent write still lands. A file opened to append makes   *)
(* its writes on the pool at its end, wherever the cursor was; once one   *)
(* landed the cursor is the end of it, and the writer appends once more    *)
(* after its seek to the start.                                            *)
(*                                                                          *)
(* Every call runs on the handle's own descriptor (src/fs/file.rs,         *)
(* `FileIo::new`; src/io/source/requests.rs, `Lease`): no copy of it is    *)
(* made, and every job holds a lease on the handle's object until it ran. *)
(* Once no task borrows the handle it ends (`Ending`): dropped, it hands   *)
(* its object over and the last lease out closes it; handed back by       *)
(* `into_inner`, it waits until no lease is out, and the caller may close  *)
(* the object at once. No call ever runs on a descriptor that closed.      *)
(*                                                                          *)
(* What is abstract: the file state's guard (a step under it runs whole), *)
(* the write-behind (the copied write is the first, so it goes in flight  *)
(* at once), and how much a job reads or writes (one cell).               *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    MayLeave,  \* whether a task may go away while its job is out
    Appending, \* whether the file was opened to append
    Ending,    \* how the handle ends once no task borrows it: "none" (it outlives the run), "drop", "into_inner"
    Mutation   \* "none", or the name of the one rule this run takes away

Readers == {"r1", "r2"}
Cells == 0..5
NoJob == [at |-> -1]

(* --algorithm LentFile
variables
    holder = "none",
    file = [i \in Cells |-> 0],     \* what the file holds: 0 before any write
    size = 0,                       \* the file's end: one past the last cell written
    cursor = 0,
    epoch = 0,
    writing = NoJob,                \* the write in flight: [at, val, lent, ran, on, part]
    lent = "none",                  \* the lent write: "none", "out", "done" or "orphan"
    jobs = [r \in Readers |-> NoJob], \* each reader's job: [at, epoch, ran, val, on]
    shut = FALSE,                   \* the shard went: its ring makes no more calls
    written = {},                   \* the writes accepted: <<cell, value>>
    taken = {},                     \* the reads that counted: <<reader, cell, value>>
    stale = FALSE,                  \* (record) a read counted a value the cell no longer holds
    redone = FALSE,                 \* (record) a read found the cursor or epoch moved and read again
    orphanLanded = FALSE,           \* (record) the write of a task that went away landed
    short = FALSE,                  \* (record) a write on the ring landed in two parts
    movedMid = FALSE,               \* (record) a write cut short on the ring was finished on the pool
    overwrote = FALSE,              \* (record) an append landed on a cell already written
    offEnd = FALSE,                 \* (record) an append landed and the cursor is not the end
    pastCursor = FALSE,             \* (record) an append landed elsewhere than the cursor it was made at
    objOpen = TRUE,                 \* the handle's object, its descriptor, is open
    gone = FALSE,                   \* the handle ended (dropped, or its object handed back)
    usedClosed = FALSE;             \* (record) a job's call ran on a descriptor that closed

define
    InFlight == writing # NoJob /\ ~writing.ran
    \* Where a new job goes: the ring of the task's shard while it runs one, or the pool.
    Wheres == IF shut THEN {"pool"} ELSE {"ring", "pool"}
    \* Where a write goes: an append always on the pool.
    WriteWheres == IF Appending THEN {"pool"} ELSE Wheres
    \* A job its runner may make now: the pool's always, the ring's while the shard runs.
    Runs(j) == j.on = "pool" \/ ~shut
    AllDone == \A p \in Readers \cup {"writer"} : pc[p] = "Done"
    \* The leases out: the write in flight and every read job not run yet each hold one.
    Leases == (IF InFlight THEN 1 ELSE 0)
              + Cardinality({r \in Readers : jobs[r] # NoJob /\ ~jobs[r].ran})
end define;

\* `poll_writes`, under the guard: a write that ran is taken out of flight, and the lent one's
\* buffer goes back to its task (or with the job, if the task went away).
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
    if writing # NoJob /\ writing.ran then
        writing := NoJob;
    end if;
end macro;

\* The blocking pool and the shard's ring, which make the jobs' calls; and the shard's going.
fair process pool = "pool"
begin
P_loop:
    while ~(AllDone /\ ~InFlight /\ \A r \in Readers : jobs[r] = NoJob \/ jobs[r].ran) do
        either
            await InFlight /\ Runs(writing);
            either
                \* All of the write, or the rest of one cut short; an append at the end.
                \* MUTATION append_at_cursor: an append lands where the cursor was.
                with cell = IF Appending /\ Mutation # "append_at_cursor" THEN size ELSE writing.at do
                    if ~objOpen then usedClosed := TRUE; end if;
                    if Appending /\ file[cell] # 0 then overwrote := TRUE; end if;
                    if Appending /\ cell # writing.at then pastCursor := TRUE; end if;
                    file[cell] := writing.val;
                    writing := [writing EXCEPT !.at = cell, !.ran = TRUE];
                    size := IF cell + 1 > size THEN cell + 1 ELSE size;
                end with;
            or
                \* The ring's call takes only part of it: half the cell holds the value.
                await writing.on = "ring" /\ writing.part = 0;
                if ~objOpen then usedClosed := TRUE; end if;
                file[writing.at] := -writing.val;
                short := TRUE;
                \* MUTATION no_continuation: a write cut short counts as done.
                if Mutation = "no_continuation" then
                    writing.ran := TRUE;
                else
                    writing.part := 1;
                end if;
            end either;
        or
            with r \in {q \in Readers : jobs[q] # NoJob /\ ~jobs[q].ran /\ Runs(jobs[q])} do
                if ~objOpen then usedClosed := TRUE; end if;
                jobs[r].val := file[jobs[r].at] || jobs[r].ran := TRUE;
            end with;
        or
            \* The shard goes: every call its ring has out is made on the pool.
            await ~shut;
            shut := TRUE;
            jobs := [r \in Readers |->
                        IF jobs[r] # NoJob /\ ~jobs[r].ran THEN [jobs[r] EXCEPT !.on = "pool"]
                        ELSE jobs[r]];
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

fair process reader \in Readers
begin
R_hold:
    await holder = "none";
    holder := self;
R_submit:
    PollWrites(-1);
    \* A read sees the writes before it: it waits for the one in flight.
    if InFlight then
        holder := "none";
R_wait_writes:
        await ~InFlight;
        goto R_hold;
    else
        with where \in Wheres do
            jobs[self] := [at |-> cursor, epoch |-> epoch, ran |-> FALSE, val |-> 0, on |-> where];
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
    \* MUTATION no_epoch_check: a result counts if only the cursor is the same.
    if jobs[self].at = cursor /\ (jobs[self].epoch = epoch \/ Mutation = "no_epoch_check") then
        if jobs[self].val # file[jobs[self].at] then stale := TRUE; end if;
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

fair process writer = "writer"
begin
W_copy:
    \* A copied write: nothing is in flight, so it goes at once.
    await holder = "none";
    with where \in WriteWheres do
        writing := [at |-> cursor, val |-> 1, lent |-> FALSE, ran |-> FALSE, on |-> where, part |-> 0];
    end with;
    written := written \cup {<<IF Appending THEN -1 ELSE cursor, 1>>};
    cursor := cursor + 1;
    epoch := epoch + 1;
W_hold:
    await holder = "none";
    holder := "writer";
W_poll:
    PollWrites(-1);
W_lend:
    \* MUTATION lent_skips_wait: the lent write goes in flight over the one there.
    if InFlight /\ Mutation # "lent_skips_wait" then
        holder := "none";
W_wait_writes:
        await ~InFlight;
        goto W_hold;
    else
        with where \in WriteWheres do
            writing := [at |-> cursor, val |-> 2, lent |-> TRUE, ran |-> FALSE, on |-> where, part |-> 0];
        end with;
        written := written \cup {<<IF Appending THEN -1 ELSE cursor, 2>>};
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
    \* A seek back to the start waits for the write in flight; the epoch moves on.
    await holder = "none";
    holder := "writer";
W_seeked:
    \* A write in flight stays in flight: the seek waits; else it goes to the start.
    PollWrites(IF InFlight THEN -1 ELSE 0);
    if InFlight then
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
    writing := [at |-> cursor, val |-> 3, lent |-> FALSE, ran |-> FALSE, on |-> "pool", part |-> 0];
    written := written \cup {<<-1, 3>>};
    cursor := cursor + 1;
    epoch := epoch + 1;
W_append_wait:
    \* A seek after it would wait for it, then take where it left the cursor.
    await holder = "none" /\ ~InFlight;
    PollWrites(-1);
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

\* A read counts only a value its cell still holds when the read is taken.
NoStaleRead == ~stale

\* No read counts a cell a write had landed only part of.
NoTornRead == \A t \in taken : t[3] >= 0

\* Once every task is done and no write is in flight, every write accepted is in the file:
\* at its cell, or anywhere for an append (-1).
AllLanded == (AllDone /\ ~InFlight) =>
    \A w \in written : IF w[1] >= 0 THEN file[w[1]] = w[2] ELSE \E c \in Cells : file[c] = w[2]

\* An append never writes over what the file holds.
NoOverwrite == ~overwrote

\* Once an append landed and was taken, the cursor is the file's end.
CursorAtEnd == ~offEnd

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
\* A job is still out when the handle was dropped: the job closes its object.
NoJobAfterDrop == ~(gone /\ Leases > 0)
=============================================================================

----------------------------- MODULE LentFile -----------------------------
(***************************************************************************)
(* File reads and writes from a task's own buffer (src/fs/file.rs:         *)
(* `read_owned`, `write_owned`, `poll_writes`), on the file's jobs.         *)
(*                                                                          *)
(* In plain words: the file has a cursor and an epoch, which every write   *)
(* and seek moves on. Jobs run on the blocking pool, in any order. Two     *)
(* tasks each read one cell at the cursor into their own buffers: a read   *)
(* waits for the write in flight, then its job reads at the cursor, and   *)
(* its result counts only if the cursor and the epoch are still those it  *)
(* read at; else it reads again. A third task writes one cell from the     *)
(* pool (a copied write), then one from its own buffer, which waits for   *)
(* the write in flight and is then the write in flight itself, then seeks *)
(* back to the start (a seek waits for the write in flight). Any task may *)
(* go away while its job is out: a read job is cancelled or its result    *)
(* dropped, and a lent write still lands.                                  *)
(*                                                                          *)
(* What is abstract: the file state's guard (a step under it runs whole), *)
(* the write-behind (the copied write is the first, so it goes in flight  *)
(* at once), and how much a job reads or writes (one cell).               *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    MayLeave,  \* whether a task may go away while its job is out
    Mutation   \* "none", or the name of the one rule this run takes away

Readers == {"r1", "r2"}
Cells == 0..3
NoJob == [at |-> -1]

(* --algorithm LentFile
variables
    holder = "none",
    file = [i \in Cells |-> 0],     \* what the file holds: 0 before any write
    cursor = 0,
    epoch = 0,
    writing = NoJob,                \* the write in flight: [at, val, lent, ran]
    lent = "none",                  \* the lent write: "none", "out", "done" or "orphan"
    jobs = [r \in Readers |-> NoJob], \* each reader's job: [at, epoch, ran, val]
    written = {},                   \* the writes accepted: <<cell, value>>
    taken = {},                     \* the reads that counted: <<reader, cell, value>>
    stale = FALSE,                  \* (record) a read counted a value the cell no longer holds
    redone = FALSE,                 \* (record) a read found the cursor or epoch moved and read again
    orphanLanded = FALSE;           \* (record) the write of a task that went away landed

define
    InFlight == writing # NoJob /\ ~writing.ran
    AllDone == \A p \in Readers \cup {"writer"} : pc[p] = "Done"
end define;

\* `poll_writes`, under the guard: a write that ran is taken out of flight, and the lent one's
\* buffer goes back to its task (or with the job, if the task went away).
macro PollWrites() begin
    if writing # NoJob /\ writing.ran then
        if writing.lent then
            if lent = "orphan" then orphanLanded := TRUE; end if;
            lent := IF lent = "out" THEN "done" ELSE "none";
        end if;
        writing := NoJob;
    end if;
end macro;

fair process pool = "pool"
begin
P_loop:
    while ~(AllDone /\ ~InFlight /\ \A r \in Readers : jobs[r] = NoJob \/ jobs[r].ran) do
        either
            await InFlight;
            file[writing.at] := writing.val;
            writing.ran := TRUE;
        or
            with r \in {q \in Readers : jobs[q] # NoJob /\ ~jobs[q].ran} do
                jobs[r].val := file[jobs[r].at] || jobs[r].ran := TRUE;
            end with;
        end either;
    end while;
end process;

fair process reader \in Readers
begin
R_hold:
    await holder = "none";
    holder := self;
R_submit:
    PollWrites();
    \* A read sees the writes before it: it waits for the one in flight.
    if InFlight then
        holder := "none";
R_wait_writes:
        await ~InFlight;
        goto R_hold;
    else
        jobs[self] := [at |-> cursor, epoch |-> epoch, ran |-> FALSE, val |-> 0];
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
    writing := [at |-> cursor, val |-> 1, lent |-> FALSE, ran |-> FALSE];
    written := written \cup {<<cursor, 1>>};
    cursor := cursor + 1;
    epoch := epoch + 1;
W_hold:
    await holder = "none";
    holder := "writer";
W_poll:
    PollWrites();
W_lend:
    \* MUTATION lent_skips_wait: the lent write goes in flight over the one there.
    if InFlight /\ Mutation # "lent_skips_wait" then
        holder := "none";
W_wait_writes:
        await ~InFlight;
        goto W_hold;
    else
        writing := [at |-> cursor, val |-> 2, lent |-> TRUE, ran |-> FALSE];
        written := written \cup {<<cursor, 2>>};
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
    PollWrites();
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
    PollWrites();
    if InFlight then
        holder := "none";
        goto W_seek;
    else
        cursor := 0;
        epoch := epoch + 1;
        holder := "none";
    end if;
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

\* Once every task is done and no write is in flight, every write accepted is in the file.
AllLanded == (AllDone /\ ~InFlight) => \A w \in written : file[w[1]] = w[2]

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
=============================================================================

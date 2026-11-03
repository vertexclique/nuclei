------------------------------ MODULE SendFile ------------------------------
(***************************************************************************)
(* A `send_file` chunk the blocking pool sends, and the stream's next      *)
(* write (src/net/send_file.rs: `send_file_in_kernel`;                     *)
(* src/io/source/write/outside.rs: `Source::send_outside`,                 *)
(* `WriteState::lend_outside`, `WriteState::end_outside`, `Left::drop`,    *)
(* `Ended::drop`; src/io/source/write/lent.rs: `push_lent`;                *)
(* src/io/source/write.rs: `WriteState::write`, `unstarve`).               *)
(*                                                                          *)
(* Who acts. A task sends a range of a file to a stream: its chunk is a    *)
(* job of the blocking pool, which calls `sendfile(2)` on the stream's     *)
(* descriptor, outside the stream's write side, since a page that is not   *)
(* cached blocks the call. The task may be dropped (a timeout) while the   *)
(* job is out. A writer then writes the handle's next bytes (an error      *)
(* frame, say) through the write side. Something else may make the side    *)
(* look at what it holds at any time (a buffer back to the pool).          *)
(*                                                                          *)
(* What can happen. Before the job goes to the pool, the task has the      *)
(* write side hold the chunk's place: a lent write that is out, which no   *)
(* driver writes, so every write after it waits. The job records its end   *)
(* when it ends (or is dropped unrun), the task records that it went when  *)
(* its future is dropped (or once it took the job's answer), and whichever *)
(* of the two comes second has the side clear the place, which looks that  *)
(* the job ended first and wakes the writes that wait.                     *)
(*                                                                          *)
(* What must never happen. A write made after the call returned or was     *)
(* dropped reaches the stream before the chunk (`WireOrder`): the stream   *)
(* would read [file prefix][frame][chunk]. And the place, once the job     *)
(* ended, must be cleared, or the writer waits for good (`FrameSent`).     *)
(*                                                                          *)
(* What is abstract. One chunk; the chunk's bytes land in one step (the    *)
(* kernel's `sendfile` puts them on the stream in order); the side's hold  *)
(* is one step around each change of it; the two bits of the call's end    *)
(* (`ENDED`, `LEFT`) are one atomic word, each set with one `fetch_or`.    *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC                                             \* numbers, sequences and TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Mutation   \* "none", or the name of the one rule this run takes away

(* --algorithm SendFile                                                       \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    wire = <<>>,                                                             \* what reached the stream, in order: "chunk", "frame"
    place = FALSE,                                                           \* the write side holds the chunk's place (a lent write, out)
    job = "none",                                                            \* the chunk's job: none, queued, running, or ended
    ended = FALSE,                                                           \* the job recorded its end (`ENDED`)
    left = FALSE,                                                            \* the task recorded that it went (`LEFT`)
    lateChunk = FALSE;                                                       \* (record) the task went with the chunk still on its way

define                                                                       \* names the properties use:
    \* What the side's clear does to the place: it goes only once the job ended.
    \* MUTATION clear_unended: the clear takes the place whatever the job is doing.
    Cleared == IF ended \/ Mutation = "clear_unended" THEN FALSE ELSE place  \* the place after a clear
end define;                                                                  \* end of the names

\* The task: it has the side hold the chunk's place, hands the job to the pool, and either takes
\* the job's answer or is dropped; either way it records that it went, as `Left::drop` does.
fair process task = "task"                                                   \* weakly fair: it keeps polling
begin                                                                        \* the task's steps:
T_start:                                                                     \* the call's first poll, or its drop before it
    either                                                                   \* it polls:
        await ~place;                                                        \* the side lends: no other lent write is held
        \* MUTATION no_place: the chunk is sent outside the side, which holds nothing, as before the fix.
        place := Mutation # "no_place";                                      \* the side holds the chunk's place
        job := "queued";                                                     \* and the job goes to the pool, in the same poll
    or                                                                       \* or it is dropped before it lent anything:
        left := TRUE;                                                        \* nothing is out, nothing to clear
        goto Done;                                                           \* the call is gone
    end either;                                                              \* the job is out
T_wait:                                                                      \* the job is out
    either                                                                   \* its answer comes:
        await job = "ended";                                                 \* the task's poll takes it
        \* MUTATION task_no_clear: the task takes the answer and records nothing, so the place stays.
        if Mutation = "task_no_clear" then goto Done; end if;                \* (the mutation's end)
    or                                                                       \* or the future is dropped (a timeout):
        if job # "ended" then lateChunk := TRUE; end if;                     \* (recorded: the chunk may still land)
        \* MUTATION clear_at_drop: the drop takes the place at once, though the job is out.
        if Mutation = "clear_at_drop" then                                   \* (the mutation)
            place := FALSE;                                                  \* the place goes now
            left := TRUE;                                                    \* and the drop records that the task went
            goto Done;                                                       \* the call is gone
        end if;                                                              \* the fix goes on below
    end either;                                                              \* the task goes, one way or the other
T_left:                                                                      \* `Left::drop`: one fetch_or of LEFT
    left := TRUE;                                                            \* the task went
    if ~ended then goto Done; end if;                                        \* the job is still out: its end clears the place
T_clear:                                                                     \* the job ended first: the task's drop clears
    \* MUTATION drop_no_clear: the drop that comes after the job's end clears nothing.
    if Mutation # "drop_no_clear" then place := Cleared; end if;             \* the side's clear, under its hold
end process;                                                                 \* end of the task

\* The pool's thread: it runs the job, which sends the chunk, then records its end, as `Ended::drop`
\* does; or the pool drops the job unrun (its runtime went), which records the end all the same.
fair process pool = "pool"                                                   \* weakly fair: a job queued is run
begin                                                                        \* the pool's steps:
P_take:                                                                      \* the job is taken off the queue
    await job = "queued" \/ (job = "none" /\ pc["task"] = "Done");           \* there is one, or there never will be
    if job = "none" then                                                     \* the call was dropped before its job:
        goto Done;                                                           \* nothing to run
    else                                                                     \* the job is there:
        either                                                               \* it runs:
            job := "running";                                                \* the call starts
        or                                                                   \* or it is dropped unrun:
            job := "ended";                                                  \* no bytes go
            \* MUTATION unrun_forgets: a job dropped unrun records no end.
            if Mutation = "unrun_forgets" then goto Done; else goto P_end; end if;  \* its end is recorded
        end either;                                                          \* the call runs
    end if;                                                                  \* taken, or nothing to take
P_send:                                                                      \* `sendfile(2)` puts the chunk on the stream
    wire := Append(wire, "chunk");                                           \* the chunk's bytes land
    job := "ended";                                                          \* and the call returns
P_end:                                                                       \* `Ended::drop`: one fetch_or of ENDED
    ended := TRUE;                                                           \* the job ended
    if ~left then goto Done; end if;                                         \* the task is still there: its drop clears
P_clear:                                                                     \* the task went first: the job's end clears
    \* MUTATION job_no_clear: the job's end, with the task gone, clears nothing.
    if Mutation # "job_no_clear" then place := Cleared; end if;              \* the side's clear, from the pool's thread
end process;                                                                 \* end of the pool

\* Something that makes the side look at what it holds (a buffer back to the pool): the same clear.
fair process look = "look"                                                   \* weakly fair: it comes, at any moment
begin                                                                        \* its one step:
L_look:                                                                      \* the side's `unstarve`
    either place := Cleared; or skip; end either;                            \* it looks, or it never comes
end process;                                                                 \* end of the look

\* The writer: the handle's next write, once the call returned or was dropped.
fair process writer = "writer"                                               \* weakly fair
begin                                                                        \* the writer's steps:
W_wait:                                                                      \* it writes after the call
    await left \/ pc["task"] = "Done";                                       \* the call returned or was dropped
W_write:                                                                     \* the write, through the side
    await ~place;                                                            \* a write waits while a lent write is held
    wire := Append(wire, "frame");                                           \* the frame goes to the stream
end process;                                                                 \* end of the writer
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* The chunk reaches the stream before the frame written after the call, never after it.
WireOrder == \A i, j \in 1..Len(wire) :                                      \* any two things on the stream:
                 (wire[i] = "frame" /\ wire[j] = "chunk") => j < i           \* a chunk comes before the frame

\* The frame goes out in the end: the chunk's place never stays held for good.
FrameSent == <>(\E i \in 1..Len(wire) : wire[i] = "frame")                   \* the writer's bytes land

\* `Termination` (every process ends) is the one the translator writes for the fair processes.

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* The task is dropped while its chunk is still on its way.
NoLateChunk == ~lateChunk                                                    \* never dropped with the job out
=============================================================================

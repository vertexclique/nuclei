-------------------------------- MODULE Locals --------------------------------
(***************************************************************************)
(* A worker's shard that goes for good (`Shard::go` in                     *)
(* src/rt/shard/going.rs) while its ring has a file operation out and it  *)
(* holds two `shard_local!` values (src/rt/shard/locals.rs), whose drops  *)
(* may panic.                                                               *)
(*                                                                          *)
(* In plain words: the shard goes in steps. It evicts its sources, then    *)
(* settles its ring: on a ring only its own thread can reap it waits for   *)
(* the operation to complete (for a bounded time: past it, the operation   *)
(* keeps its memory for good), and on a ring another thread can reap it    *)
(* cancels the operation and, at the end, hands the ring and what is still *)
(* out to a keeper. Only then are the shard-local values dropped, each in  *)
(* a catch of its own (`drop_locals`): a panic is logged and the next      *)
(* value still goes. Then the shard drops its tasks and lets go of its     *)
(* ring. The kernel completes the operation whenever it likes, writing     *)
(* into the operation's buffer first. A panic that unwinds out of `go`     *)
(* skips the rest of it, and the shard's fields go at once: the ring, with *)
(* the operation and its buffer.                                           *)
(*                                                                          *)
(* What must never happen: the kernel writes into a freed buffer; a panic  *)
(* aborts the process; the shard's going stops short of its end; a value's *)
(* panic goes untold, or a value is never dropped; a value's drop (user    *)
(* code) runs while the ring still owes the kernel a settle.               *)
(*                                                                          *)
(* What is abstract: the evictions (Going.tla checks them), the tasks (one *)
(* step), the ring's one operation, and the values' drops (one step that   *)
(* drops both).                                                            *)
(*                                                                          *)
(* `Mutation` names one rule taken away (see README).                      *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Reapable,  \* TRUE: another thread can reap the ring, so the shard cancels and hands it on
    Panics,    \* how many of the two values panic as they are dropped: 0, 1 or 2
    Mutation   \* "none", "no_catch", "one_catch", "values_first" or "before"

(* --algorithm Locals
variables
    opOut = TRUE,         \* the kernel holds the operation and may write into its buffer
    bufFreed = FALSE,     \* the operation's buffer was freed
    leaked = FALSE,       \* the settle's wait ran out: the operation keeps its memory for good
    settled = FALSE,      \* the shard settled its ring (waited, or cancelled on a reapable one)
    ring = "shard",       \* who holds the ring: "shard", "keeper", or "gone" (dropped)
    dropped = 0,          \* the values dropped
    told = 0,             \* the values' panics logged
    aborted = FALSE,      \* (record) the process aborted
    unwound = FALSE,      \* (record) a panic unwound out of the shard's going
    tasksDropped = FALSE, \* the shard dropped its tasks
    wroteFreed = FALSE,   \* (record) the kernel wrote into a freed buffer
    droppedEarly = FALSE; \* (record) a value's drop ran before the ring was settled

define
    \* The kernel never writes into a buffer that was freed.
    NoWriteIntoFreed == ~wroteFreed
    \* No panic aborts the process.
    NoAbort == ~aborted
    \* A shard that is done went all the way: its tasks were dropped.
    GoesToTheEnd == pc["S"] = "Done" => tasksDropped
    \* A shard that is done dropped both values and logged every panic of theirs.
    ValuesTold == pc["S"] = "Done" => (dropped = 2 /\ told = Panics)
    \* The values' drops, which are user code, run only once the ring is settled.
    ValuesAfterSettle == ~droppedEarly
end define;

\* Drops the two values (`drop_locals`). Each goes in a catch of its own: a panic is logged and
\* the next value still goes.
\* MUTATION no_catch: no catch, so a panic unwinds out of the shard's going.
\* MUTATION one_catch: one catch around both: the first panic unwinds through the second value,
\*   whose drop runs during that unwind, and a second panic there aborts the process.
macro dropValues() begin
    dropped := 2;                                      \* both values' drops run
    droppedEarly := ~settled;                          \* (record) before the settle?
    if Mutation \in {"no_catch", "before"} /\ Panics > 0 then
        unwound := TRUE;                               \* the panic goes on unwinding
    elsif Mutation = "one_catch" /\ Panics = 2 then
        aborted := TRUE;                               \* a panic during an unwind aborts
    else
        told := Panics;                                \* each panic caught and logged
    end if;
end macro;

fair process shard = "S"
begin
S_evict:
    \* The sources are evicted (their operations cancelled through the mailbox).
    \* MUTATION values_first: the values go first, before the ring settles, as they once did.
    \* MUTATION before: the code before: the values first, and no catch.
    if Mutation \in {"values_first", "before"} then dropValues(); end if;
S_unwound_early:
    \* A panic that unwound skips the rest of the going.
    if unwound then goto S_unwind; end if;
S_settle:
    \* The ring settles: a reapable one has its operation cancelled (the kernel ends it soon);
    \* any other waits for the kernel to complete it, or, past a bounded wait, leaves it with its
    \* memory for good.
    if Reapable then
        settled := TRUE;
    else
        either
            await ~opOut;                              \* completed within the wait
        or
            leaked := TRUE;                            \* the wait ran out
        end either;
        settled := TRUE;
    end if;
S_values:
    \* The values go now, the ring settled.
    if Mutation \notin {"values_first", "before"} then dropValues(); end if;
S_unwound_late:
    if unwound then goto S_unwind; end if;
S_tasks:
    \* The shard's tasks are dropped (one step here).
    tasksDropped := TRUE;
S_keep:
    \* The ring goes to a keeper with the operation still out, or is dropped; an operation the
    \* settle left for good keeps its memory.
    if Reapable /\ opOut then
        ring := "keeper";
    else
        ring := "gone";
    end if;
    goto Done;
S_unwind:
    \* The panic unwound out of the going: the shard's fields go at once, the ring with them, and
    \* an operation still out, unless the settle left it for good, loses its buffer.
    ring := "gone";
    if opOut /\ ~leaked then bufFreed := TRUE; end if;
end process;

fair process kernel = "K"
begin
K_complete:
    \* The kernel completes the operation when it likes: it writes into the buffer first, then
    \* posts the completion.
    if bufFreed then wroteFreed := TRUE; end if;
    opOut := FALSE;
end process;

fair process keeper = "P"
begin
P_wait:
    \* A keeper waits for a ring handed to it, or for the shard to be done without one.
    await ring = "keeper" \/ pc["S"] = "Done";
    if ring = "keeper" then
P_reap:
        \* It reaps the operation's completion, then frees the buffer and the ring.
        await ~opOut;
        bufFreed := TRUE;
        ring := "gone";
    end if;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

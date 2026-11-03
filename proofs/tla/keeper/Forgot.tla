------------------------------- MODULE Forgot -------------------------------
(***************************************************************************)
(* An operation a going shard gives up on, and what waits for it.         *)
(*                                                                          *)
(* The Rust it models: on a ring no keeper can take (single issuer with    *)
(* deferred task work: only its own thread can reap it), the shard's drop  *)
(* (`Shard::go`, src/rt/shard/going.rs) calls `ShardRing::shut_down`, which *)
(* cancels what is out (`cancel_all`), waits a bounded time for it         *)
(* (`settle`), and gives up on what is still out (`forget_stuck`), in      *)
(* src/rt/shard/ring/keeper.rs; a source operation's end is `op_done`      *)
(* (src/io/source/ops.rs) and a file call's is its slot (`FileOp::forsake`  *)
(* in src/fs/ring.rs, `Slot` in src/fs/call.rs); `Source::retire`          *)
(* (src/io/source/requests.rs) waits until none of the source's            *)
(* operations is out.                                                      *)
(*                                                                          *)
(* In plain words: the shard goes with one operation in the kernel. It     *)
(* asks the kernel to cancel it and waits, but only so long: the kernel    *)
(* may complete the operation in time (it is applied, and its memory goes  *)
(* back), or not (the operation ignores the cancel, or is slow). One still *)
(* out when the wait ends is given up on: its memory is kept for good,     *)
(* since the kernel may still write it, and the ring goes. A task waits    *)
(* for the operation's end (its side's, or its file call's slot), and      *)
(* another thread retires the source (`get_mut`, `into_inner`), which      *)
(* waits until none of its operations is counted out. The operation given  *)
(* up on ends for both: its end is an error, and it is no longer counted.  *)
(* A zero-copy send whose first completion came ended for its side then,  *)
(* and was counted out: it stays in the ring only to hold its buffer until *)
(* its notification, so one given up on keeps its buffer and ends nothing.  *)
(*                                                                          *)
(* What must never happen: the kernel writing the operation's memory after *)
(* it was freed (NoScribble), and the operation ending twice (EndsOnce);   *)
(* and everything must end (Ends): the task's wait and the retire.         *)
(*                                                                          *)
(* A file's call (with `File`: a write, a sync, a read of a file) cannot   *)
(* be held up by a peer: it ignores the cancel but ends in its own time,   *)
(* which a slow device may stretch past the short wait. So the shard waits *)
(* for the files' calls until they all ended, bounded by the runtime's     *)
(* linger time (`Builder::write_behind_linger`), not by the short wait     *)
(* that bounds a source's (`settle_files` beside `settle`). A file's call  *)
(* the kernel ends within the linger time (`Within`) is never given up;   *)
(* one a device holds past it is. Whatever is given up is said at warn and *)
(* counted (`Stats::operations_given_up`): never silently (`GivenUpSaid`). *)
(*                                                                          *)
(* What is abstract: the length of the bounded waits (the shard's clock    *)
(* moves from the short wait to the linger time and past it at any moment, *)
(* and past the linger time only once a file's call that ends within it    *)
(* ended), and what the operation is beyond a file's call or a source's.   *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Stuck,     \* the operation ignores the cancel: the kernel ends it only after the shard gave up, if ever
    Held,      \* the operation ended for its side already, and only holds its memory (a zero-copy send)
    File,      \* the operation is a file's call: it ignores the cancel but ends in its own time
    Within,    \* a file's call ends within the linger time
    Mutation   \* "none", or the name of the one rule this run takes away

VARIABLES
    op,        \* the operation: "kernel" (held), "posted" (completed, not applied), "applied", "late"
    spc,       \* the going shard's place: "cancel" "settle" "gone"
    out,       \* the source's count of its operations in the kernel's hands (`ops_out`)
    ends,      \* how many times the operation's end reached its side or slot
    freed,     \* the operation's memory was freed
    scribbled, \* (record) the kernel wrote that memory after it was freed
    rpc,       \* the retire on another thread: "ask" "wait" "done"
    tpc,       \* the task waiting for the operation's end: "wait" "done"
    clock,     \* the going shard's wait: "short" (the short wait), "linger" (past it), "past" (past the linger time)
    given,     \* the operation was given up on
    said       \* the give-up was said at warn and counted

vars == <<op, spc, out, ends, freed, scribbled, rpc, tpc, clock, given, said>>   \* every variable

Init ==
    /\ op = "kernel"           \* the operation is in the kernel
    /\ spc = "cancel"          \* the shard is going
    /\ out = IF Held THEN 0 ELSE 1   \* counted out on its source, unless its end came
    /\ ends = IF Held THEN 1 ELSE 0  \* it has not ended, unless it only holds its memory
    /\ freed = FALSE           \* its memory is in place
    /\ scribbled = FALSE       \* nothing wrote freed memory
    /\ rpc = "ask"             \* the retire is about to be asked
    /\ tpc = "wait"            \* the task waits
    /\ clock = "short"         \* the short wait has begun
    /\ given = FALSE           \* nothing given up
    /\ said = FALSE            \* nothing said

(***************************************************************************)
(* The going shard.                                                        *)
(***************************************************************************)

\* `cancel_all`: the kernel is asked to cancel what is out.
SCancel ==
    /\ spc = "cancel"                                       \* the shard is going
    /\ spc' = "settle"                                      \* it waits next
    /\ UNCHANGED <<op, out, ends, freed, scribbled, rpc, tpc, clock, given, said>>

\* The wait applies the operation's completion: its side gets the end, the count falls, and its
\* memory goes back (a held send's notification only lets go of its memory). Nothing is out
\* then, and the ring goes.
SApply ==
    /\ spc = "settle"                                       \* the shard waits
    /\ op = "posted"                                        \* and the completion came
    /\ op' = "applied"                                      \* it is applied
    /\ out' = IF Held THEN out ELSE out - 1                 \* not counted out any more
    /\ ends' = IF Held THEN ends ELSE ends + 1              \* the end reaches its side
    /\ freed' = TRUE                                        \* the memory goes back
    /\ spc' = "gone"                                        \* the ring goes
    /\ UNCHANGED <<scribbled, rpc, tpc, clock, given, said>>

\* The bounded wait ends with the operation still in the kernel (`forget_stuck`): its memory is
\* kept for good, and its end is an error, which counts it out.
\* MUTATION forget_silent: the operation is forgotten with no end and still counted (the code
\* before): its task and the retire wait for good.
\* MUTATION forget_frees: its end lets go of its memory too, which the kernel may still write.
\* MUTATION end_twice: a held send given up on ends for its side again (and counts out again).
\* A source's operation is given up on once the short wait ran out; a file's call only once the
\* linger time ran out. The give-up is said at warn and counted.
\* MUTATION file_short_wait: a file's call is given up on once the short wait ran out (the code before).
\* MUTATION forget_unsaid: the give-up is neither said nor counted where anyone sees (the code before).
SGiveUp ==
    /\ spc = "settle"                                       \* the shard waits
    /\ op = "kernel"                                        \* and the operation is still out
    /\ IF File /\ Mutation # "file_short_wait"              \* a file's call
       THEN clock = "past"                                  \* waits out the linger time
       ELSE clock # "short"                                 \* a source's the short wait
    /\ spc' = "gone"                                        \* the wait ran out: the ring goes
    /\ given' = TRUE                                        \* the operation is given up on
    /\ said' = (Mutation # "forget_unsaid")                 \* said and counted
    /\ LET again == ~Held \/ Mutation = "end_twice"         \* its end is still to come (or the bug)
       IN IF Mutation = "forget_silent"                     \* the planted bug
          THEN UNCHANGED <<out, ends, freed>>               \* nothing ends, nothing counts it out
          ELSE /\ out' = IF again THEN out - 1 ELSE out     \* not counted out any more
               /\ ends' = IF again THEN ends + 1 ELSE ends  \* the error reaches its side or slot
               /\ freed' = (Mutation = "forget_frees")      \* the memory stays, but in the bug
    /\ UNCHANGED <<op, scribbled, rpc, tpc, clock>>

\* The shard's clock: the short wait runs out at some moment, and later the linger time; a file's
\* call that ends within the linger time ends before it runs out.
Tick ==
    /\ spc = "settle"                                       \* while the shard waits
    /\ \/ clock = "short" /\ clock' = "linger"              \* the short wait runs out
       \/ /\ clock = "linger"                               \* the linger time runs out,
          /\ ~(File /\ Within /\ op = "kernel")             \* never before a file's call ends within it
          /\ clock' = "past"
    /\ UNCHANGED <<op, spc, out, ends, freed, scribbled, rpc, tpc, given, said>>

(***************************************************************************)
(* The kernel.                                                             *)
(***************************************************************************)

\* The kernel ends the operation, writing its memory as it does (a receive's bytes). One that
\* ignores the cancel ends only after the shard gave up on it, if ever; its completion then
\* reaches nobody.
KernelEnd ==
    /\ op = "kernel"                                        \* still in the kernel
    /\ ~Stuck \/ spc = "gone"                               \* not held back by the shard
    /\ op' = IF spc = "gone" THEN "late" ELSE "posted"      \* completed: for nobody, or for the wait
    /\ scribbled' = (scribbled \/ freed)                    \* (record) its memory had been freed
    /\ UNCHANGED <<spc, out, ends, freed, rpc, tpc, clock, given, said>>

(***************************************************************************)
(* The retire and the task.                                                *)
(***************************************************************************)

\* Another thread retires the source: its request is performed at once (no side is held).
RAsk ==
    /\ rpc = "ask"                                          \* about to retire
    /\ rpc' = "wait"                                        \* it waits for the operations
    /\ UNCHANGED <<op, spc, out, ends, freed, scribbled, tpc, clock, given, said>>

\* The retire returns once none of the source's operations is counted out.
RWait ==
    /\ rpc = "wait"                                         \* the retire waits
    /\ out = 0                                              \* until nothing is out
    /\ rpc' = "done"                                        \* and returns
    /\ UNCHANGED <<op, spc, out, ends, freed, scribbled, tpc, clock, given, said>>

\* The task waiting for the operation's end gets it.
TWait ==
    /\ tpc = "wait"                                         \* the task waits
    /\ ends > 0                                             \* until the end came
    /\ tpc' = "done"                                        \* and goes on
    /\ UNCHANGED <<op, spc, out, ends, freed, scribbled, rpc, clock, given, said>>

(***************************************************************************)
(* The whole system.                                                       *)
(***************************************************************************)

Next ==
    \/ SCancel \/ SApply \/ SGiveUp \/ Tick                  \* the going shard and its clock
    \/ KernelEnd                                             \* the kernel
    \/ RAsk \/ RWait \/ TWait                                \* the retire and the task

\* Every step that must happen does, if it stays possible. A kernel that ignores the cancel may
\* never end the operation.
Fairness ==
    /\ WF_vars(SCancel) /\ WF_vars(SApply) /\ WF_vars(SGiveUp) /\ WF_vars(Tick)
    /\ WF_vars(RAsk) /\ WF_vars(RWait) /\ WF_vars(TWait)
    /\ (~Stuck => WF_vars(KernelEnd))

Spec == Init /\ [][Next]_vars /\ Fairness                   \* the behaviours checked

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* The kernel never writes the operation's memory after it was freed.
NoScribble == ~scribbled

\* The operation ends at most once for its side or slot, and its count never goes below zero.
EndsOnce == ends <= 1 /\ out >= 0

\* The task gets the operation's end, and the retire returns.
Ends == <>(tpc = "done" /\ rpc = "done")

\* A file's call the kernel ends within the linger time is never given up on.
FileNotGivenUp == (File /\ Within) => ~given

\* Whatever is given up on is said at warn and counted.
GivenUpSaid == given => said

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* The operation is given up on while the kernel still holds it.
NoGivenUp == ~(spc = "gone" /\ op = "kernel" /\ ends = 1)
\* A slow operation completes after the shard gave up on it.
NoLate == op # "late"
=============================================================================

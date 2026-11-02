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
(*                                                                          *)
(* What must never happen: the kernel writing the operation's memory after *)
(* it was freed (NoScribble), and the operation ending twice (EndsOnce);   *)
(* and everything must end (Ends): the task's wait and the retire.         *)
(*                                                                          *)
(* What is abstract: the length of the bounded wait (its end is a step     *)
(* that may come at any moment while the operation is out), and what the  *)
(* operation is: a receive, a send, a file's call.                         *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Stuck,     \* the operation ignores the cancel: the kernel ends it only after the shard gave up, if ever
    Mutation   \* "none", or the name of the one rule this run takes away

VARIABLES
    op,        \* the operation: "kernel" (held), "posted" (completed, not applied), "applied", "late"
    spc,       \* the going shard's place: "cancel" "settle" "gone"
    out,       \* the source's count of its operations in the kernel's hands (`ops_out`)
    ends,      \* how many times the operation's end reached its side or slot
    freed,     \* the operation's memory was freed
    scribbled, \* (record) the kernel wrote that memory after it was freed
    rpc,       \* the retire on another thread: "ask" "wait" "done"
    tpc        \* the task waiting for the operation's end: "wait" "done"

vars == <<op, spc, out, ends, freed, scribbled, rpc, tpc>>   \* every variable

Init ==
    /\ op = "kernel"           \* the operation is in the kernel
    /\ spc = "cancel"          \* the shard is going
    /\ out = 1                 \* counted out on its source
    /\ ends = 0                \* it has not ended
    /\ freed = FALSE           \* its memory is in place
    /\ scribbled = FALSE       \* nothing wrote freed memory
    /\ rpc = "ask"             \* the retire is about to be asked
    /\ tpc = "wait"            \* the task waits

(***************************************************************************)
(* The going shard.                                                        *)
(***************************************************************************)

\* `cancel_all`: the kernel is asked to cancel what is out.
SCancel ==
    /\ spc = "cancel"                                       \* the shard is going
    /\ spc' = "settle"                                      \* it waits next
    /\ UNCHANGED <<op, out, ends, freed, scribbled, rpc, tpc>>

\* The wait applies the operation's completion: its side gets the end, the count falls, and its
\* memory goes back. Nothing is out then, and the ring goes.
SApply ==
    /\ spc = "settle"                                       \* the shard waits
    /\ op = "posted"                                        \* and the completion came
    /\ op' = "applied"                                      \* it is applied
    /\ out' = out - 1                                       \* not counted out any more
    /\ ends' = ends + 1                                     \* the end reaches its side
    /\ freed' = TRUE                                        \* the memory goes back
    /\ spc' = "gone"                                        \* the ring goes
    /\ UNCHANGED <<scribbled, rpc, tpc>>

\* The bounded wait ends with the operation still in the kernel (`forget_stuck`): its memory is
\* kept for good, and its end is an error, which counts it out.
\* MUTATION forget_silent: the operation is forgotten with no end and still counted (the code
\* before): its task and the retire wait for good.
\* MUTATION forget_frees: its end lets go of its memory too, which the kernel may still write.
SGiveUp ==
    /\ spc = "settle"                                       \* the shard waits
    /\ op = "kernel"                                        \* and the operation is still out
    /\ spc' = "gone"                                        \* the wait ran out: the ring goes
    /\ IF Mutation = "forget_silent"                        \* the planted bug
       THEN UNCHANGED <<out, ends, freed>>                  \* nothing ends, nothing counts it out
       ELSE /\ out' = out - 1                               \* not counted out any more
            /\ ends' = ends + 1                             \* the error reaches its side or slot
            /\ freed' = (Mutation = "forget_frees")         \* the memory stays, but in the bug
    /\ UNCHANGED <<op, scribbled, rpc, tpc>>

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
    /\ UNCHANGED <<spc, out, ends, freed, rpc, tpc>>

(***************************************************************************)
(* The retire and the task.                                                *)
(***************************************************************************)

\* Another thread retires the source: its request is performed at once (no side is held).
RAsk ==
    /\ rpc = "ask"                                          \* about to retire
    /\ rpc' = "wait"                                        \* it waits for the operations
    /\ UNCHANGED <<op, spc, out, ends, freed, scribbled, tpc>>

\* The retire returns once none of the source's operations is counted out.
RWait ==
    /\ rpc = "wait"                                         \* the retire waits
    /\ out = 0                                              \* until nothing is out
    /\ rpc' = "done"                                        \* and returns
    /\ UNCHANGED <<op, spc, out, ends, freed, scribbled, tpc>>

\* The task waiting for the operation's end gets it.
TWait ==
    /\ tpc = "wait"                                         \* the task waits
    /\ ends > 0                                             \* until the end came
    /\ tpc' = "done"                                        \* and goes on
    /\ UNCHANGED <<op, spc, out, ends, freed, scribbled, rpc>>

(***************************************************************************)
(* The whole system.                                                       *)
(***************************************************************************)

Next ==
    \/ SCancel \/ SApply \/ SGiveUp                          \* the going shard
    \/ KernelEnd                                             \* the kernel
    \/ RAsk \/ RWait \/ TWait                                \* the retire and the task

\* Every step that must happen does, if it stays possible. A kernel that ignores the cancel may
\* never end the operation.
Fairness ==
    /\ WF_vars(SCancel) /\ WF_vars(SApply) /\ WF_vars(SGiveUp)
    /\ WF_vars(RAsk) /\ WF_vars(RWait) /\ WF_vars(TWait)
    /\ (~Stuck => WF_vars(KernelEnd))

Spec == Init /\ [][Next]_vars /\ Fairness                   \* the behaviours checked

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* The kernel never writes the operation's memory after it was freed.
NoScribble == ~scribbled

\* The operation ends at most once for its side or slot.
EndsOnce == ends <= 1

\* The task gets the operation's end, and the retire returns.
Ends == <>(tpc = "done" /\ rpc = "done")

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* The operation is given up on while the kernel still holds it.
NoGivenUp == ~(spc = "gone" /\ op = "kernel" /\ ends = 1)
\* A slow operation completes after the shard gave up on it.
NoLate == op # "late"
=============================================================================

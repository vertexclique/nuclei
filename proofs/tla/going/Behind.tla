-------------------------------- MODULE Behind --------------------------------
(***************************************************************************)
(* Bytes a source's writes took into its write-behind, when the shard that *)
(* drove them goes (`Shard::leave` and `drain_unsent` in                   *)
(* src/rt/shard/going.rs, `Source::poll_drain` in                          *)
(* src/io/source/write/calls.rs, `WriteState::submit_ops` and `op_done` in *)
(* src/io/source/write/ops.rs, `attach_port` and `waits_for_ops` in        *)
(* src/io/source/port.rs).                                                 *)
(*                                                                          *)
(* Who acts. Shard A is the shard of a thread whose outermost `block_on`   *)
(* just returned: a write of a task there was taken into the write-behind, *)
(* and the handle lives on (another thread holds it). The kernel ends an   *)
(* operation that carries some of the bytes, if one is out. A worker W     *)
(* keeps running: its driver issues what a source registered with it      *)
(* holds, and a drain task may run on it. (With no worker the task runs on *)
(* A's own thread at its next `block_on`: that thread is then W.)          *)
(*                                                                          *)
(* What can happen. A goes and evicts the source: its registration goes.  *)
(* An operation out stays out (a completion port never cancels a TCP send) *)
(* or is cancelled and may come back undone, its bytes back with the side *)
(* (a ring). A hands a source that still holds bytes to a drain task on    *)
(* W. The task looks each time it is woken: with nothing left it ends;     *)
(* with the source registered nowhere and nothing out it registers the     *)
(* source with W; with an operation still out it waits, for a handle with *)
(* an operation out stays on its port, and that operation's end wakes it. *)
(* Every completion wakes the side's waiters.                              *)
(*                                                                          *)
(* What must never happen. A byte a write took never reaches the kernel   *)
(* once A has gone, though W runs; or the drain task waits for good.       *)
(*                                                                          *)
(* What is abstract. Bytes are counted, not named; an operation carries   *)
(* all the side holds; the side's guard, the port's keeper (which delivers *)
(* the packets of A's port once A has gone) and a readiness poller's edge  *)
(* (the first comes as W registers the source) are one step each.          *)
(***************************************************************************)
EXTENDS Integers, TLC                                                        \* numbers and TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Total,      \* bytes the writes took before A went
    OpOut,      \* TRUE: one of them is out with an operation as A goes (a port, a ring); FALSE: all wait (epoll, kqueue)
    Cancels,    \* TRUE: the eviction cancels the operation out, which may come back undone (a ring)
    Mutation    \* "none", or the name of the one rule this run takes away

(* --algorithm Behind                                                         \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    behind = IF OpOut THEN Total - 1 ELSE Total,                             \* bytes the side holds that no operation carries
    inflight = IF OpOut THEN 1 ELSE 0,                                       \* bytes out with an operation
    reg = "A",                                                               \* the shard the source is registered with: "A", "W", or "none"
    drain = "none",                                                          \* the drain task: "none", "waiting", or "done"
    woken = FALSE,                                                           \* the drain task was woken since it last looked
    sent = 0;                                                                \* bytes the kernel took

define                                                                       \* names the properties use:
    Holds == behind > 0 \/ inflight > 0                                      \* the side still has bytes the kernel has not taken
end define;                                                                  \* end of the names

\* Shard A: its thread's outermost block_on returned (`Shard::leave`).
fair process shardA = "A"                                                    \* weakly fair, like every process
begin                                                                        \* A's steps:
A_evict:                                                                     \* `evict_all`: the eviction, waited for
    reg := "none";                                                           \* the source is registered nowhere now
A_hand:                                                                      \* `drain_unsent`: a source that holds bytes
    \* MUTATION no_drain: A leaves the bytes to whoever polls the side next.
    if Holds /\ Mutation # "no_drain" then                                   \* still holds bytes:
        drain := "waiting";                                                  \* a drain task is spawned on W
        woken := TRUE;                                                       \* and polls once as it starts
    end if;                                                                  \* A is done
end process;                                                                 \* end of A

\* The kernel: the operation out ends (on A's port, A's keeper delivers its packet).
fair process kernel = "K"                                                    \* weakly fair: an operation out ends in the end
begin                                                                        \* the kernel's steps:
K_loop:                                                                      \* the next end
    while TRUE do                                                            \* for every operation
        await inflight > 0;                                                  \* one is out
        if Cancels /\ reg # "W" then                                         \* cancelled by the eviction:
            either sent := sent + inflight;                                  \* too late, the bytes went
            or behind := behind + inflight;                                  \* or undone: they come back to the side
            end either;                                                      \* one of the two
        else                                                                 \* not cancelled:
            sent := sent + inflight;                                         \* the bytes went
        end if;                                                              \* the operation ended
        inflight := 0;                                                       \* nothing out now
        \* MUTATION no_wake: the end wakes nobody.
        if Mutation # "no_wake" then woken := TRUE; end if;                  \* `op_done` wakes the side's waiters
    end while;                                                               \* the next one
end process;                                                                 \* end of the kernel

\* W's driver: what a source registered with W holds goes out (`submit_ops`, or the writes on an edge).
fair process driver = "W"                                                    \* weakly fair: W keeps running
begin                                                                        \* the driver's steps:
W_loop:                                                                      \* the next operation
    while TRUE do                                                            \* for as long as W runs
        await reg = "W" /\ behind > 0 /\ inflight = 0;                       \* registered here, bytes held, nothing out
        inflight := behind;                                                  \* one operation carries them all
        behind := 0;                                                         \* none left behind it
    end while;                                                               \* the next one
end process;                                                                 \* end of the driver

\* The drain task on W: `poll_drain`, polled each time it is woken.
fair process task = "D"                                                      \* weakly fair: a woken task runs in the end
begin                                                                        \* the task's steps:
D_loop:                                                                      \* the next poll
    while drain # "done" do                                                  \* until it ends
        await drain = "waiting" /\ woken;                                    \* spawned, and woken since its last poll
        woken := FALSE;                                                      \* this poll takes the wake
        if ~Holds then                                                       \* the kernel took every byte:
            drain := "done";                                                 \* the task ends
        \* MUTATION no_register: the poll leaves the source registered nowhere.
        elsif reg = "none" /\ inflight = 0 /\ Mutation # "no_register" then  \* registered nowhere, nothing out:
            reg := "W";                                                      \* `ensure_registered`: W's driver has it now
        end if;                                                              \* else an operation out wakes it as it ends
    end while;                                                               \* the task ended
end process;                                                                 \* end of the task
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* The values every variable may take.
TypeOK == /\ behind \in 0..Total                                             \* bytes held
          /\ inflight \in 0..Total                                           \* bytes out
          /\ reg \in {"A", "W", "none"}                                      \* the registration
          /\ drain \in {"none", "waiting", "done"}                           \* the drain task
          /\ sent \in 0..Total                                               \* bytes the kernel took

\* No byte is lost or sent twice: every byte a write took is held, out, or taken by the kernel.
Conserved == behind + inflight + sent = Total                                \* the sum stays the bytes taken

\* Every byte a write took reaches the kernel in the end, though the shard that drove it has gone.
AllSent == <>(sent = Total)                                                  \* the kernel takes them all

\* A drain task that was spawned ends in the end.
DrainEnds == (drain = "waiting") ~> (drain = "done")                         \* it never waits for good

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* The drain task moves the source to W.
NoMove == reg # "W"                                                          \* never registered with W
=============================================================================

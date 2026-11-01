---------------------------- MODULE LentDatagram ----------------------------
(***************************************************************************)
(* Owned datagram receives of two tasks on one socket                      *)
(* (src/io/source/read.rs: `ReadState::read_owned_from`, `reclaim`;        *)
(* src/io/source/read/ops.rs: `ReadState::op_done`, a datagram received    *)
(* ahead).                                                                 *)
(*                                                                          *)
(* Who acts. Tasks A and B each receive one datagram into a buffer of      *)
(* their own (`recv_owned`). The kernel holds two datagrams. The driver    *)
(* takes a datagram out of the kernel into the buffer lent to the side,   *)
(* or, through an operation it had out before any buffer was lent (on a    *)
(* ring or a completion port: `Ahead`), into a buffer of its own, queued   *)
(* ahead for the tasks.                                                    *)
(*                                                                          *)
(* What can happen. A task holding its buffer copies a datagram queued     *)
(* ahead into it; finding none, it lends the buffer to the side if no      *)
(* other buffer is lent, and else waits its turn. A task whose buffer is   *)
(* lent takes it back once the driver received into it; or, when a         *)
(* datagram came ahead while the lent buffer was still empty, takes its    *)
(* buffer back to copy that datagram in.                                   *)
(*                                                                          *)
(* What must never happen. A task gets back a buffer that is not its own   *)
(* (another task's, whose own is then lost), a buffer goes missing, or a  *)
(* datagram is lost or taken twice.                                        *)
(*                                                                          *)
(* What is abstract. The side's guard (a poll's steps run while it holds   *)
(* `holder`, one step for the driver's receive); a datagram's bytes (its   *)
(* name); the wakes (a task waits until it can move, weak fairness         *)
(* standing for the wake that comes then).                                 *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC                                \* numbers, sequences, sets and TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Ahead,     \* TRUE: the driver may queue a datagram ahead, a lent buffer waiting or not (a ring, a port)
    Mutation   \* "none", or the name of the one rule this run takes away

Tasks == {"A", "B"}                                                          \* the two receiving tasks, each buffer named after its task
Datagrams == {"d1", "d2"}                                                    \* the two datagrams the kernel holds

(* --algorithm LentDatagram                                                   \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    holder = "none",                                                         \* who holds the side's guard: a task, or "none"
    kernel = <<"d1", "d2">>,                                                 \* datagrams in the kernel, oldest first
    queue = <<>>,                                                            \* datagrams the driver received ahead, oldest first
    lent = "none",                                                           \* the buffer lent to the side (named after its task), or "none"
    lentHolds = "none",                                                      \* the datagram the driver received into the lent buffer, or "none"
    held = [t \in Tasks |-> t],                                              \* the buffer each task holds, "none" while its own is lent
    got = [t \in Tasks |-> <<"none", "none">>],                              \* what each task's receive gave back: <<buffer, datagram>>
    lost = FALSE;                                                            \* (record) a buffer a task held was dropped for another

define                                                                       \* names the steps and properties use:
    \* Whether task `t` has something to do when it polls: holding a buffer, a datagram is queued or no
    \* other buffer is lent; its own lent, the driver filled it or a datagram came ahead.
    CanMove(t) == IF held[t] # "none"                                        \* it holds a buffer:
                  THEN queue # <<>> \/ lent = "none"                         \*   a datagram to copy in, or room to lend it
                  ELSE lentHolds # "none" \/ queue # <<>>                    \*   its own is lent: filled, or one came ahead
    \* How many places hold datagram `d`: the kernel, the queue, the lent buffer, the tasks' results.
    Places(d) == Len(SelectSeq(kernel, LAMBDA x : x = d))                    \* in the kernel
               + Len(SelectSeq(queue, LAMBDA x : x = d))                     \* queued ahead
               + (IF lentHolds = d THEN 1 ELSE 0)                            \* in the lent buffer
               + Cardinality({t \in Tasks : got[t][2] = d})                  \* given back to a task
end define;                                                                  \* end of the names

\* The driver: one datagram at a time out of the kernel, while the lent buffer is empty.
fair process driver = "driver"                                               \* weakly fair: a datagram there is taken in the end
begin                                                                        \* the driver's steps:
D_loop:                                                                      \* the next datagram
    while kernel # <<>> do                                                   \* until the kernel holds none
        \* One step, the guard free: into the lent buffer, or (an operation out before the lend) queued ahead.
        await holder = "none" /\ lentHolds = "none" /\ (lent # "none" \/ Ahead);  \* the side free, somewhere to put it
        if lent # "none" /\ Ahead then                                       \* both are possible:
            either                                                           \* the driver's receive was the lend's
                lentHolds := Head(kernel);                                   \* the datagram lands in the lent buffer
            or                                                               \* or one out before the lend brought it
                queue := Append(queue, Head(kernel));                        \* queued ahead, in a buffer of the driver's
            end either;                                                      \* one of the two
        elsif lent # "none" then                                             \* a lent buffer and no operation of the driver's:
            lentHolds := Head(kernel);                                       \* the datagram lands in the lent buffer
        else                                                                 \* nothing lent, an operation of the driver's out:
            queue := Append(queue, Head(kernel));                            \* queued ahead
        end if;                                                              \* the datagram has its place
        kernel := Tail(kernel);                                              \* and is out of the kernel
    end while;                                                               \* the next one
end process;                                                                 \* end of the driver

\* A task's owned receive: polled each time it can move, under the guard, until it has a datagram.
fair process task \in Tasks                                                  \* weakly fair: a task that can move polls in the end
begin                                                                        \* the task's steps:
T_take:                                                                      \* a poll begins
    await holder = "none" /\ CanMove(self);                                  \* the guard free, and something to do (its wake)
    holder := self;                                                          \* the poll holds the guard
T_reclaim:                                                                   \* `reclaim`, first: a datagram came ahead meanwhile
    \* MUTATION no_held_check: a task holding its own buffer takes the lent one back too.
    if (held[self] = "none" \/ Mutation = "no_held_check")                   \* only the task whose buffer is lent
       /\ queue # <<>> /\ lent # "none" /\ lentHolds = "none" then           \* a datagram ahead, the lent buffer empty:
        if held[self] # "none" then lost := TRUE; end if;                    \* the buffer it held would be dropped
        held[self] := lent;                                                  \* the lent buffer comes back to this task
        lent := "none";                                                      \* and is lent no more
    end if;                                                                  \* the reclaim is done
T_use:                                                                       \* the receive proper
    if held[self] # "none" then                                              \* it holds a buffer:
        if queue # <<>> then                                                 \* a datagram queued ahead:
            got[self] := <<held[self], Head(queue)>>;                        \* copied into the buffer it holds, given back
            queue := Tail(queue);                                            \* and out of the queue
        elsif lent = "none" then                                             \* nothing queued, nothing lent:
            lent := held[self];                                              \* the buffer is lent to the side
            held[self] := "none";                                            \* and the task holds none meanwhile
        end if;                                                              \* else another's is lent: this one waits its turn
    elsif lentHolds # "none" then                                            \* its own is lent and the driver filled it:
        got[self] := <<lent, lentHolds>>;                                    \* the buffer comes back with the datagram
        held[self] := lent;                                                  \* the task holds it again
        lent := "none";                                                      \* lent no more
        lentHolds := "none";                                                 \* and empty of the datagram it gave back
    end if;                                                                  \* the poll is done
T_release:                                                                   \* the poll lets go of the guard
    holder := "none";                                                        \* the side is free
    if got[self][1] = "none" then goto T_take; end if;                       \* not done: it polls again when it can move
end process;                                                                 \* end of a task
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* Every receive that gave something back gave the task its own buffer.
OwnBuffer == \A t \in Tasks : got[t][1] \in {"none", t}                      \* never another task's

\* No buffer a task held is dropped for another's.
NoLostBuffer == ~lost                                                        \* every buffer stays with its task or the side

\* Every datagram is in exactly one place: never lost, never taken twice.
Conserved == \A d \in Datagrams : Places(d) = 1                              \* one place each

\* `Termination` (every process ends) is the one the translator writes for the fair processes.

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A datagram comes ahead while a buffer waits lent and empty: the case the reclaim is for.
NoAheadWhileLent == ~(queue # <<>> /\ lent # "none" /\ lentHolds = "none")   \* never both at once
=============================================================================

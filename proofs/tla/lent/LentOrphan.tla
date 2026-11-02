----------------------------- MODULE LentOrphan -----------------------------
(***************************************************************************)
(* An owned datagram receive whose task goes away (src/io/source/read.rs:  *)
(* `ReadState::abandon_lent`, and the receive into the lent buffer:        *)
(* `receive_lent` on epoll and kqueue, `RingMem::RecvLentFrom` on a ring,  *)
(* `PortMem::RecvLentFrom` on a port; src/io/source/read/ops.rs `op_done`  *)
(* hands an orphan's end to `abandon_lent`).                               *)
(*                                                                          *)
(* Who acts. A task receives datagrams one owned receive after another,    *)
(* each into a buffer of its own that it lends to the side; it may go away *)
(* (its future dropped) before it takes the datagram back. The driver      *)
(* receives the socket's next datagram into the lent buffer, cut to it if  *)
(* the datagram is longer, and the kernel says whether it cut it           *)
(* (MSG_TRUNC on unix, STATUS_BUFFER_OVERFLOW on a port). Later a receive  *)
(* into a buffer that holds any datagram takes what is left.               *)
(*                                                                          *)
(* What can happen. A task that takes its buffer back gets the datagram as *)
(* the driver received it, cut to its own buffer as the kernel cuts it. A  *)
(* lent buffer whose task went away keeps what the driver received: a      *)
(* datagram that came whole goes on to the next receive; one the kernel    *)
(* cut is dropped and counted, for the next receive would take its first   *)
(* bytes for the whole of it.                                              *)
(*                                                                          *)
(* What must never happen. A later receive gets a datagram cut to a buffer *)
(* that was not its own. A whole datagram is dropped (one that fills the   *)
(* lent buffer exactly is whole). A datagram is lost unaccounted or taken  *)
(* twice.                                                                  *)
(*                                                                          *)
(* What is abstract. Sizes are units; the guard (each step runs whole      *)
(* under it); the wakes (a task waits until it can move).                  *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC                                             \* numbers, sequences, TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Mutation    \* "none", or the name of the one rule this run takes away

Cap == 2                                                                     \* the units each lent buffer holds
Sizes == <<2, 3, 1>>                                                         \* the datagrams, in order: one that fills it exactly, one longer, one shorter
N == Len(Sizes)                                                              \* how many datagrams there are
Rounds == 2                                                                  \* how many owned receives the task makes
Min(a, b) == IF a < b THEN a ELSE b                                          \* the smaller of two sizes

(* --algorithm LentOrphan                                                      \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    kernel = <<1, 2, 3>>,  \* the datagrams the socket holds, oldest first, by number
    lent = FALSE,          \* a buffer is lent to the side
    orphan = FALSE,        \* the task whose buffer is lent went away
    filled = <<>>,         \* what the driver received into the lent buffer: <<number, units, cut>>
    ahead = <<>>,          \* datagrams handed on to the next receive: <<number, units>>
    mine = <<>>,           \* what the task's owned receives took back: <<number, units>>
    later = <<>>,          \* what the later receive took: <<number, units>>
    dropped = <<>>,        \* datagrams dropped because the kernel cut them, by number (counted)
    handedOn = FALSE,      \* (record) a whole datagram of a task that went was handed on
    rounds = 0;            \* owned receives the task made

define                                                                       \* names the steps and properties use:
    \* `abandon_lent` hands a filled buffer's datagram on: only if it came whole.
    \* MUTATION pass_cut: it hands it on whatever, as it did before.
    \* MUTATION full_as_cut: it takes a datagram that filled the buffer for a cut one.
    HandsOn(f) == CASE Mutation = "pass_cut" -> TRUE                         \* (mutated) always
                    [] Mutation = "full_as_cut" -> f[2] < Cap                \* (mutated) only one shorter than the buffer
                    [] OTHER -> ~f[3]                                        \* only one the kernel did not cut
    \* How many places hold datagram d.
    In(s, d) == Len(SelectSeq(s, LAMBDA x : x[1] = d))                       \* in a sequence of pairs
    Places(d) == Len(SelectSeq(kernel, LAMBDA x : x = d))                    \* in the socket
               + (IF filled # <<>> /\ filled[1] = d THEN 1 ELSE 0)           \* in the lent buffer
               + In(ahead, d) + In(mine, d) + In(later, d)                   \* handed on, or taken
               + Len(SelectSeq(dropped, LAMBDA x : x = d))                   \* dropped and counted
end define;                                                                  \* end of the names

\* `abandon_lent` of a lent buffer whose task went, once the driver filled it.
macro abandon() begin                                                        \* the filled buffer of a task that went
    if HandsOn(filled) then                                                  \* a whole datagram:
        ahead := Append(ahead, <<filled[1], filled[2]>>);                    \* the next receive takes it
        handedOn := TRUE;                                                    \* (record)
    else                                                                     \* a cut one:
        dropped := Append(dropped, filled[1]);                               \* dropped, and counted
    end if;                                                                  \* the buffer is let go
    filled := <<>>;                                                          \* empty
    lent := FALSE;                                                           \* and lent no more
    orphan := FALSE;                                                         \* with no task to go
end macro;                                                                   \* end of the abandon

\* The driver: the next datagram into the lent buffer, cut to it, the kernel saying whether it cut it.
fair process driver = "driver"                                               \* weakly fair: a datagram there is taken in the end
begin                                                                        \* the driver's steps:
D_recv:                                                                      \* one receive
    while TRUE do                                                            \* for as long as the run goes
        await lent /\ filled = <<>> /\ kernel # <<>>;                        \* a buffer waits, and a datagram is there
        filled := <<Head(kernel), Min(Sizes[Head(kernel)], Cap), Sizes[Head(kernel)] > Cap>>; \* received, cut or not
        kernel := Tail(kernel);                                              \* out of the socket
D_end:                                                                       \* the receive's end, under the guard
        if orphan /\ filled # <<>> then abandon(); end if;                   \* its task went: `op_done` abandons it
    end while;                                                               \* the next one
end process;                                                                 \* end of the driver

\* The task: owned receives, one after another, each of which may be given up.
fair process task = "task"                                                   \* weakly fair, like every process
begin                                                                        \* the task's steps:
T_lend:                                                                      \* an owned receive
    while rounds < Rounds do                                                 \* while it makes more
        lent := TRUE;                                                        \* its buffer is lent to the side
        rounds := rounds + 1;                                                \* one more receive
T_wait:                                                                      \* it waits for the datagram
        either                                                               \* it takes its buffer back:
            await filled # <<>>;                                             \* filled
            mine := Append(mine, <<filled[1], filled[2]>>);                  \* the datagram, cut to its own buffer
            filled := <<>>;                                                  \* empty again
            lent := FALSE;                                                   \* and its own
        or                                                                   \* or it goes away first:
            if filled # <<>> then                                            \* the driver filled it already:
                abandon();                                                   \* `abandon_lent` now
            else                                                             \* still empty:
                either                                                       \* a ring's or a port's receive holds it:
                    orphan := TRUE;                                          \* the receive's end abandons it
                or                                                           \* nothing holds it (epoll, kqueue):
                    lent := FALSE;                                           \* it is let go at once, empty
                end either;                                                  \* the buffer is dealt with
            end if;                                                          \* the receive is given up
        end either;                                                          \* the receive is over
T_settle:                                                                    \* the next receive waits for the side
        await ~lent;                                                         \* the lent buffer let go
    end while;                                                               \* the next receive
end process;                                                                 \* end of the task

\* The later receive: once the task is done, into a buffer that holds any datagram.
fair process reader = "reader"                                               \* weakly fair, like every process
begin                                                                        \* the reader's steps:
R_wait:                                                                      \* it waits for the task's receives
    await rounds = Rounds /\ ~lent;                                          \* all made, none lent
R_read:                                                                      \* one receive
    while ahead # <<>> \/ kernel # <<>> do                                   \* while a datagram is left
        if ahead # <<>> then                                                 \* one handed on comes first:
            later := Append(later, Head(ahead));                             \* taken as it was handed on
            ahead := Tail(ahead);                                            \* and gone from there
        else                                                                 \* else the socket's next:
            later := Append(later, <<Head(kernel), Sizes[Head(kernel)]>>);   \* taken whole
            kernel := Tail(kernel);                                          \* out of the socket
        end if;                                                              \* one datagram taken
    end while;                                                               \* every datagram taken
end process;                                                                 \* end of the reader
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* The later receive gets every datagram whole: none cut to a buffer that was not its own.
NoCutPassed == \A j \in 1..Len(later) : later[j][2] = Sizes[later[j][1]]     \* never a cut one

\* Only a datagram the kernel cut is dropped: one that fills the buffer exactly is whole.
NoWholeDropped == \A j \in 1..Len(dropped) : Sizes[dropped[j]] > Cap         \* never a whole one

\* Every datagram is in exactly one place: the socket, the lent buffer, handed on, taken, or dropped and counted.
Conserved == \A d \in 1..N : Places(d) = 1                                   \* one place each

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A cut datagram of a task that went is dropped and counted.
NoDropped == dropped = <<>>                                                  \* never so
\* A whole datagram of a task that went is handed on to the later receive.
NoHandedOn == ~handedOn                                                      \* never so
=============================================================================

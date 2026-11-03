------------------------------ MODULE PortLook ------------------------------
(***************************************************************************)
(* A task reading a socket on a completion port whose pool has no buffer   *)
(* for it, so it reads straight from the socket once a zero-byte receive   *)
(* (a look) said bytes came (src/io/source/read.rs `read_starved`,         *)
(* `receive_starved` and `taken_by_another`; the look is `Want::Ready` in  *)
(* src/io/source/read/ops.rs `submit_ops`).                                *)
(*                                                                          *)
(* Who acts. A peer sends two chunks, one after the other. The task reads  *)
(* one chunk. A call outside the side (std's own read through `get_ref`,   *)
(* or another handle on the same socket) may take a chunk at any time.    *)
(* The port completes the look once the socket holds bytes, and its end    *)
(* wakes the task.                                                         *)
(*                                                                          *)
(* What can happen. A task that finds the side readable reads from the     *)
(* socket itself, on a socket made non-blocking: it takes a chunk, or,     *)
(* when the outside call took the chunk first, finds none                 *)
(* (`WSAEWOULDBLOCK`).                                                     *)
(* A task that finds none marks the side unreadable, sends another look,   *)
(* and waits for it, as for the first. A task that finds the side not      *)
(* readable and no look out sends one and waits.                           *)
(*                                                                          *)
(* What must never happen. A read answers an error for a chunk another     *)
(* call took (the socket is fine). A task waits for good while the socket  *)
(* holds its chunk. Before the socket was non-blocking, that read would    *)
(* have blocked the shard's thread.                                        *)
(*                                                                          *)
(* What is abstract. The guard (each step runs whole under it); a chunk is *)
(* its number.                                                             *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC                                             \* numbers, sequences, TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Mutation    \* "none", or the name of the one rule this run takes away

Tasks == {"A"}                                                               \* the reading task

(* --algorithm PortLook                                                        \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    sent = 0,                \* chunks the peer sent
    socket = <<>>,           \* chunks the socket holds, oldest first
    look = FALSE,            \* a look is out with the port
    readable = FALSE,        \* a look said bytes came, and no read took them since
    waiting = {},            \* the tasks waiting for the side
    got = [t \in Tasks |-> 0],   \* the chunk each task read, 0 for none yet
    erred = FALSE,           \* (record) a read answered an error for a chunk another took
    taken = FALSE,           \* (record) a read found the chunk a look announced taken
    stolen = 0;              \* chunks the outside call took

define                                                                       \* names the properties use:
    Done(t) == got[t] # 0                                                    \* the task has its chunk
end define;                                                                  \* end of the names

\* The peer sends its two chunks.
fair process peer = "peer"                                                   \* weakly fair, like every process
begin                                                                        \* the peer's steps:
P_send:                                                                      \* one chunk
    while sent < 2 do                                                        \* while some are left
        sent := sent + 1;                                                    \* the next chunk
        socket := Append(socket, sent);                                      \* reaches the socket
    end while;                                                               \* both sent
end process;                                                                 \* end of the peer

\* The port: a look out ends once the socket holds bytes, and its end wakes every task waiting.
fair process port = "port"                                                   \* weakly fair: a look ends in the end
begin                                                                        \* the port's steps:
L_end:                                                                       \* one look's end
    while TRUE do                                                            \* for as long as the run goes
        await look /\ socket # <<>>;                                         \* a look out, and bytes there
        look := FALSE;                                                       \* it ends
        readable := TRUE;                                                    \* saying bytes came
        waiting := {};                                                       \* and the tasks waiting are woken
    end while;                                                               \* the next look
end process;                                                                 \* end of the port

\* The outside call: std's own read through `get_ref`, which takes a chunk past the side, once at most.
fair process outside = "outside"                                             \* weakly fair, like every process
begin                                                                        \* its steps:
O_take:                                                                      \* it may take a chunk
    either                                                                   \* now, if one is there,
        await socket # <<>>;                                                 \* a chunk on the socket
        socket := Tail(socket);                                              \* taken past the side
        stolen := 1;                                                         \* (counted)
    or                                                                       \* or never
        skip;                                                                \* it takes none
    end either;                                                              \* the call is over
end process;                                                                 \* end of the outside call

\* A task: one read, made again each time it is woken until it has its chunk.
fair process task \in Tasks                                                  \* weakly fair, like every process
begin                                                                        \* the task's steps:
T_read:                                                                      \* a poll of the read
    while ~Done(self) do                                                     \* until it has its chunk
        await self \notin waiting;                                           \* woken, or never waiting
        if readable then                                                     \* a look said bytes came:
            if socket # <<>> then                                            \* the task's own read takes a chunk
                got[self] := Head(socket);                                   \* its chunk
                socket := Tail(socket);                                      \* off the socket
                readable := FALSE;                                           \* the next read looks again
            else                                                             \* the outside call took it: WSAEWOULDBLOCK
                taken := TRUE;                                               \* (record)
                readable := FALSE;                                           \* the side is not readable now
                \* MUTATION would_block_errors: the read answers the WouldBlock as an error.
                if Mutation = "would_block_errors" then                      \* (mutated) an error to the task
                    erred := TRUE;                                           \* (record)
                \* MUTATION would_block_waits: the task waits with no look sent for it.
                elsif Mutation = "would_block_waits" then                    \* (mutated) it waits
                    waiting := waiting \union {self};                        \* for a wake nobody sends
                else                                                         \* `taken_by_another`:
                    look := TRUE;                                            \* another look goes out
                    waiting := waiting \union {self};                        \* and the task waits for it
                end if;                                                      \* the read is over
            end if;                                                          \* one call made
        else                                                                 \* not readable:
            look := TRUE;                                                    \* a look goes out, or stays
            waiting := waiting \union {self};                                \* and the task waits for its end
        end if;                                                              \* the poll is over
    end while;                                                               \* it has its chunk
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

\* No read answers an error for a chunk the other task took.
NoSpuriousError == ~erred                                                    \* never so

\* The task reads a chunk in the end.
Served == <>(\A t \in Tasks : Done(t))                                       \* served

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A task's own read finds the chunk the look announced taken by the outside call.
NoTaken == ~taken                                                            \* never so
=============================================================================

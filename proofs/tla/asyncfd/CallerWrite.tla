---------------------------- MODULE CallerWrite ----------------------------
(***************************************************************************)
(* A socket's write side turning from the handle's own writes to its      *)
(* caller's calls (src/io/source/write.rs `fd_ready` and `fd_clear`;       *)
(* `Source::fd_clear_write` in src/io/source/write/calls.rs), at the grain *)
(* of each atomic access.                                                  *)
(*                                                                          *)
(* In plain words: the task writes bytes through the handle (the          *)
(* write-behind), which the driver hands to the socket a few at a time.   *)
(* Then the task sends bytes of its own straight to the socket; until the *)
(* write-behind is empty it waits, the driver still writing. The socket   *)
(* holds `Room` bytes the peer has not read; the peer reads them one at a  *)
(* time, and each read frees room and brings an edge that marks the side   *)
(* ready and counts it. The task waits until the side is ready, notes the *)
(* count (its tick) and sends: room, the byte goes; none (`WouldBlock`),   *)
(* it clears the side's ready only if the count is still its tick, since  *)
(* an edge after its call is the only one it will get. The peer must get  *)
(* every byte once, in the order the task wrote them.                     *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC                                             \* numbers, sequences, TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Behind,    \* bytes written through the handle
    Own,       \* bytes the task sends itself after them
    Room,      \* bytes the socket holds that the peer has not read yet
    Mutation   \* "none", or the name of the one rule this run takes away

(* --algorithm CallerWrite
variables
    behind = <<>>,      \* the handle's write-behind
    got = <<>>,         \* what reached the socket, in order
    sent = 0,           \* bytes the task wrote so far, through the handle and its own
    sock = 0,           \* bytes in the socket the peer has not read
    ready = TRUE,       \* the side says the socket may take bytes (it starts so)
    edges = 0;          \* the side's count of what made it ready

\* The peer reads one byte at a time: room frees, and the poller reports it with an edge.
fair process peer = "peer"
begin
P_loop:
    while TRUE do
P_read:
        await sock > 0;                                                      \* a byte to read
        sock := sock - 1;                                                    \* read: one byte of room
        ready := TRUE;                                                       \* the edge: the side may be ready
        edges := edges + 1;                                                  \* and counts it
    end while;
end process;

fair process driver = "driver"
begin
D_loop:
    while TRUE do
D_flush:
        \* The driver hands the oldest byte behind to the socket, while it has room.
        await behind # <<>> /\ sock < Room;
        got := Append(got, Head(behind));
        behind := Tail(behind);
        sock := sock + 1;
    end while;
end process;

fair process task = "task"
variables tick = 0;     \* the count its last look saw
begin
T_write:
    behind := [i \in 1..Behind |-> i];                                       \* written through the handle
    sent := Behind;
T_own:
    while sent < Behind + Own do
T_wait:
        \* MUTATION no_flush_wait: the call goes out with bytes still behind.
        await behind = <<>> \/ Mutation = "no_flush_wait";
T_ready:
        await ready;                                                         \* the side may take a byte
        tick := edges;                                                       \* the look's tick
T_send:
        if sock < Room then                                                  \* room: the byte goes
            got := Append(got, sent + 1);
            sent := sent + 1;
            sock := sock + 1;
        else                                                                 \* `WouldBlock`; the peer may read now
T_clear:
            \* MUTATION no_write_tick: the clear does not look at the count, and an edge after the call is lost.
            if edges = tick \/ Mutation = "no_write_tick" then               \* no edge since the look:
                ready := FALSE;                                              \* the side waits for the next
            end if;
        end if;
    end while;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

\* The socket gets every byte once, in the order the task wrote them.
OutOrder == \A i \in 1..Len(got) : got[i] = i

\* Every byte reaches the socket.
AllOut == <>(Len(got) = Behind + Own)
=============================================================================

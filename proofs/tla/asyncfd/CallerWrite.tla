---------------------------- MODULE CallerWrite ----------------------------
(***************************************************************************)
(* A socket's write side turning from the handle's own writes to its      *)
(* caller's calls (src/io/source/write.rs `fd_ready`): what the handle    *)
(* wrote behind goes out before a call of the caller's, at the grain of   *)
(* each atomic access.                                                     *)
(*                                                                          *)
(* In plain words: the task writes bytes through the handle (the          *)
(* write-behind), which the driver hands to the socket a few at a time.   *)
(* Then the task sends bytes of its own straight to the socket; until the *)
(* write-behind is empty it waits, the driver still writing. The peer     *)
(* must get every byte once, in the order the task wrote them.            *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    Behind,    \* bytes written through the handle
    Own,       \* bytes the task sends itself after them
    Mutation   \* "none", or "no_flush_wait"

(* --algorithm CallerWrite
variables
    behind = <<>>,      \* the handle's write-behind
    got = <<>>,         \* what reached the socket, in order
    sent = 0;

fair process driver = "driver"
begin
D_loop:
    while TRUE do
D_flush:
        \* The driver hands the oldest byte behind to the socket.
        await behind # <<>>;
        got := Append(got, Head(behind));
        behind := Tail(behind);
    end while;
end process;

fair process task = "task"
begin
T_write:
    behind := [i \in 1..Behind |-> i];
    sent := Behind;
T_own:
    while sent < Behind + Own do
T_wait:
        \* MUTATION no_flush_wait: the call goes out with bytes still behind.
        await behind = <<>> \/ Mutation = "no_flush_wait";
T_send:
        got := Append(got, sent + 1);
        sent := sent + 1;
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

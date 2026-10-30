------------------------------- MODULE Sender -------------------------------
(***************************************************************************)
(* A datagram receive that names its sender (src/io/sys/unix.rs:          *)
(* `unix_addr`, `unnamed`; src/io/source/read.rs: `receive_ahead`,         *)
(* `receive_lent`; src/io/source/ring.rs for the ring's receives).         *)
(*                                                                          *)
(* In plain words: a peer sends datagrams over a Unix socket pair, whose   *)
(* ends have no name. A receive takes one from the kernel, and then turns  *)
(* the address the kernel wrote into the sender's address for the task.   *)
(* Meanwhile the process may run out of descriptors (other code opens     *)
(* files), and get them back. Once the kernel handed a datagram over, it  *)
(* is the receive's: it must reach the task, so naming its sender must    *)
(* never fail. Making the unnamed address by opening a socket (as the     *)
(* code once did) fails with no descriptor free, and the datagram is gone. *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    Total,     \* the datagrams the peer sends, numbered 1..Total
    Mutation   \* "none", or "name_opens_socket": naming the unnamed sender opens a socket

(* --algorithm Sender
variables
    kernel = <<>>,     \* the datagrams the kernel holds for the socket, oldest first
    sent = 0,          \* how many the peer sent
    free = TRUE,       \* the process has a descriptor free
    taken = <<>>,      \* the datagram the receive took from the kernel, if any
    got = <<>>,        \* the datagrams the task received, in order
    failed = 0,        \* receives that answered an error
    starvedTake = FALSE; \* (record) a datagram was named while no descriptor was free

define
    \* Whether naming the sender works now: with no socket to open, always.
    NameWorks == Mutation # "name_opens_socket" \/ free
end define;

\* The peer: sends its datagrams one by one.
fair process peer = "peer"
begin
P_send:
    \* One datagram at a time, numbered in order.
    while sent < Total do
        \* The next datagram goes to the kernel.
        sent := sent + 1;
        \* The kernel queues it for the socket.
        kernel := Append(kernel, sent);
    end while;
end process;

\* Other code of the process: takes the last descriptors, and gives them back.
fair process other = "other"
begin
O_take:
    \* It opens files until none is free.
    free := FALSE;
O_give:
    \* It closes them again.
    free := TRUE;
end process;

\* The task's receives, until it got every datagram.
fair process receiver = "receiver"
begin
R_loop:
    \* Receive after receive.
    while Len(got) + failed < Total do
        \* A receive waits for a datagram in the kernel.
        await kernel # <<>>;
        \* The kernel hands the oldest over: it is no longer the socket's.
        taken := <<Head(kernel)>>;
        \* The kernel's queue loses it.
        kernel := Tail(kernel);
R_name:
        \* Record a receive that names its sender while no descriptor is free.
        if ~free then starvedTake := TRUE; end if;
        \* The sender's address is made for the task.
        if NameWorks then
            \* The task gets the datagram.
            got := got \o taken;
        else
            \* MUTATION name_opens_socket: no socket to open; the receive answers the error.
            failed := failed + 1;
        end if;
        \* The receive holds nothing now.
        taken := <<>>;
    end while;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* No datagram is lost: each is in the kernel, with the receive, or with the task.
Conservation == Len(got) + Len(taken) + Len(kernel) = sent

\* The task gets the datagrams in the order they were sent, as a Unix socket keeps them.
Order == \A i \in 1..(Len(got) - 1) : got[i] < got[i + 1]

(***************************************************************************)
(* Witness: claims an interesting case never happens and must be proved    *)
(* wrong.                                                                  *)
(***************************************************************************)

\* A receive names an unnamed sender while the process has no descriptor free.
NoStarvedTake == ~starvedTake
=============================================================================

----------------------------- MODULE RingSend -----------------------------
(***************************************************************************)
(* A socket's sends on an io_uring ring, at the grain of a step under the  *)
(* side's guard: a datagram socket's (src/io/source/write/datagram.rs      *)
(* `WriteState::send_now`), or a stream's (src/io/source/write.rs          *)
(* `write_direct`, `wait_for_room` and `write`); the poll each waits on in *)
(* `submit_ops` and its end in `op_done`, src/io/source/write/ops.rs. On   *)
(* epoll and kqueue the same call waits for the socket's edge instead of   *)
(* the poll (`serve` wakes it, and writes the write-behind out in          *)
(* `push_fd`): the poll's end here stands for either. On a Windows         *)
(* completion port (`Backend = "port"`, `poll_send` in write.rs and        *)
(* `send_queued` in write/datagram.rs) a datagram is copied and queued for *)
(* the port and its call answers taken at once; one too large for any      *)
(* socket is refused at once (`datagram_too_long`), and any other refusal  *)
(* the kernel makes of it reaches the next call.                           *)
(*                                                                          *)
(* In plain words: a task sends datagrams one call after another. The     *)
(* first may be its own buffer lent to a send the ring makes (`send_owned` *)
(* of a large one); the task may go away while it is out, and then that    *)
(* datagram still goes. Every other call is a borrowed send: when nothing  *)
(* of the side is out, the task hands its datagram to the kernel itself,   *)
(* which answers at once: taken, refused (too large for any socket), or no *)
(* room. With no room the call waits for a poll the ring makes, which ends *)
(* when the socket has room, and tries again. A peer reads the datagrams   *)
(* the socket holds, oldest first; one from before fills the socket at the *)
(* start.                                                                  *)
(*                                                                          *)
(* A stream's task writes chunks the same way instead (`Stream`): small    *)
(* ones and large ones (16 KiB or more, `BEHIND_MIN`). A chunk goes to the *)
(* kernel from the task's own buffer when nothing of the side is ahead of  *)
(* it and the socket has room. A small one that cannot is copied into the  *)
(* write-behind and taken at once, and the ring sends the write-behind out *)
(* once nothing else is out. A large one is never copied: it waits for     *)
(* what is ahead of it to go out, or for the poll when the socket is full, *)
(* and then tries again (before, it was copied too, and a send of every    *)
(* chunk stayed out with the ring, so every later chunk was copied as      *)
(* well).                                                                  *)
(*                                                                          *)
(* What must never happen: a call answers anything but the kernel's own    *)
(* verdict on its datagram (the bug the size sweep found: a datagram too   *)
(* large for the kernel was taken as sent, its error left for the next     *)
(* call, as a port's refusal still is); the task's datagrams or chunks     *)
(* reach the peer out of the order                                         *)
(* it sent them; a call waits forever; a large chunk is copied.            *)
(*                                                                          *)
(* What is abstract: the side's guard (a step here runs whole under it;    *)
(* RingOp.tla checks the hand-off to the ring's thread), and the bytes: a  *)
(* datagram or a chunk is just its number, and the kernel takes a chunk    *)
(* whole.                                                                  *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    Backend,   \* "unix" (an io_uring ring, epoll, kqueue) or "port" (a Windows completion port)
    Lent,      \* TRUE: the first datagram is lent to a send of the ring's, and its task may go
    Stream,    \* TRUE: the task writes a stream's chunks instead of sending datagrams
    Mutation   \* "none", or the name of the one rule this run takes away

\* What each datagram is, in call order: "ok", or "big" (no socket takes it). A refused one
\* between two the kernel takes; with no lent send, one more, so a borrowed call meets a full
\* socket after one it filled itself.
\* A port's: one too large, refused at once, and one the kernel refuses only as it sends it ("unreach",
\* no route), between two it takes.
Datagrams == IF Backend = "port" THEN <<"ok", "big", "unreach", "ok">>
             ELSE IF Lent THEN <<"ok", "big", "ok">> ELSE <<"ok", "big", "ok", "ok">>
\* What each chunk of a stream is, in write order: "small", which may go into the write-behind,
\* or "large", which never does. A small one first, which the full socket sends behind; a large
\* one while that is still ahead of it; then one of each again.
Chunks == <<"small", "large", "small", "large">>
\* What the task sends, in order.
Sends == IF Stream THEN Chunks ELSE Datagrams
\* How many datagrams or chunks the socket holds.
Room == 1

\* How many datagrams or chunks the task sends.
N == Len(Sends)
\* Whether the kernel refuses datagram k whatever room there is.
Big(k) == Sends[k] = "big"
\* Whether the kernel refuses datagram k: too large, or with no route as it sends it.
Fails(k) == Sends[k] \in {"big", "unreach"}
\* Whether the call copies its datagram and queues it, answering taken: a port's, or (mutated) any.
Queues == Mutation = "behind" \/ Backend = "port"
\* Whether chunk k is large: it waits for the socket rather than being copied.
Large(k) == Sends[k] = "large"
\* The kernel's own verdict on datagram or chunk k, which its call must answer.
Verdict(k) == IF Fails(k) THEN "err" ELSE "ok"

(* --algorithm RingSend
variables
    socket = <<0>>,                    \* datagrams the socket holds for the peer, oldest first
    wire = <<>>,                       \* datagrams the peer read, in order
    out = 0,                            \* the side's operation with the ring: 0 none, -1 the poll, -2 the
                                        \* write-behind's send, k datagram k's send
    pollFirst = FALSE,                 \* the side waits for a poll that says the socket has room
    woken = FALSE,                     \* the waiting call was woken
    queued = 0,                        \* (mutation behind) a datagram copied and queued for the ring
    deferred = "none",                 \* (mutation behind) an error kept for the next call
    result = [k \in 1..N |-> "none"],  \* what each call answered
    orphaned = FALSE,                  \* the lending task went before its send ended
    behind = <<>>,                     \* (stream) chunks copied into the write-behind, not sent yet
    sending = <<>>,                    \* (stream) the write-behind's chunks a send of the ring's holds
    again = FALSE,                     \* (record) a call found no room and waited for the poll
    behindLent = FALSE,                \* (record) a call waited behind the lent send of a task that went
    waitedSend = FALSE,                \* (record) a large chunk waited for the write-behind ahead of it
    copied = FALSE;                    \* (record) a small chunk was copied into the write-behind

\* The peer reads the oldest datagram the socket holds.
fair process peer = "peer"
begin
P_read:
    while TRUE do
        \* Something to read: it comes off the socket onto the wire.
        await socket # <<>>;
        wire := Append(wire, Head(socket));
        socket := Tail(socket);
    end while;
end process;

\* The ring: it hands the kernel what the side wants, and applies each end.
fair process ring = "ring"
begin
R_loop:
    while TRUE do
        either
            \* The side waits for room and nothing is out: the ring makes the poll.
            await pollFirst /\ out = 0;
            out := -1;
        or
            \* (mutation behind) A datagram copied and queued goes to the kernel.
            await queued # 0 /\ out = 0;
            out := queued;
            queued := 0;
        or
            \* The poll ends once the socket has room: the side stops waiting for it.
            await out = -1 /\ Len(socket) < Room;
            pollFirst := FALSE;
            out := 0;
            \* MUTATION no_wake: the poll's end wakes nobody.
            if Mutation # "no_wake" then
                woken := TRUE;
            end if;
        or
            \* A datagram's send: refused at once if too large, else taken once there is room.
            await out \in 1..N /\ (Fails(out) \/ Len(socket) < Room);
            if ~Fails(out) then
                \* The kernel takes it: the socket holds it behind the others.
                socket := Append(socket, out);
            end if;
            if Lent /\ out = 1 then
                \* The lent send: its call gets the verdict, unless its task went.
                if ~orphaned then
                    result[1] := Verdict(1);
                end if;
            elsif Fails(out) then
                \* A queued datagram's error, kept for the next call (a port's; mutation behind).
                deferred := "err";
            end if;
            \* The send ended; every call waiting on the side is woken.
            out := 0;
            woken := TRUE;
        or
            \* (stream) Nothing is out and the side waits for no poll: the write-behind moves into a
            \* send of the ring's, and writes after it start another.
            await behind # <<>> /\ out = 0 /\ ~pollFirst;
            sending := behind;
            behind := <<>>;
            out := -2;
        or
            \* (stream) The write-behind's send: the kernel takes its chunks once there is room.
            await out = -2 /\ Len(socket) < Room;
            socket := socket \o sending;
            sending := <<>>;
            out := 0;
            \* MUTATION no_wake_send: the send's end wakes nobody.
            if Mutation # "no_wake_send" then
                woken := TRUE;
            end if;
        end either;
    end while;
end process;

\* The task: its calls, one after another.
fair process task = "task"
variables at = 1;   \* the datagram of the call under way
begin
T_loop:
    while at <= N do
        if Stream /\ Large(at) then
T_large:
            \* MUTATION direct_ahead: the large chunk goes to the kernel with the write-behind ahead.
            if (out # 0 \/ behind # <<>>) /\ Mutation # "direct_ahead" then
                \* What the side holds goes first, or its poll for room is out: the write waits for
                \* that, its chunk not copied. (A call before the ring made the poll it asked for
                \* only finds the socket full again; on epoll and kqueue the call waits for the edge
                \* instead, which this allows too.)
                if behind # <<>> \/ out = -2 then
                    waitedSend := TRUE;
                end if;
                woken := FALSE;
T_ahead:
                await woken;
            elsif Len(socket) < Room then
                \* The kernel takes it straight from the task's buffer.
                socket := Append(socket, at);
                result[at] := "ok";
                at := at + 1;
            elsif Mutation = "copy_large" then
                \* MUTATION copy_large: with no room the large chunk is copied into the write-behind
                \* and taken at once, as before.
                behind := Append(behind, at);
                result[at] := "ok";
                at := at + 1;
            else
                \* No room: the write asks for the poll and waits for it, then tries again.
                \* MUTATION no_poll: the write waits without asking for the poll.
                if Mutation # "no_poll" then
                    pollFirst := TRUE;
                end if;
                again := TRUE;
                woken := FALSE;
T_full:
                await woken;
            end if;
        elsif Stream then
T_small:
            if out = 0 /\ behind = <<>> /\ Len(socket) < Room then
                \* Nothing of the side is ahead and the socket has room: the kernel takes it.
                socket := Append(socket, at);
                result[at] := "ok";
                at := at + 1;
            elsif behind = <<>> then
                \* It cannot go now: it is copied into the write-behind and taken at once.
                behind := <<at>>;
                copied := TRUE;
                result[at] := "ok";
                at := at + 1;
            else
                \* The write-behind has no room: the write waits for the ring's send to end.
                woken := FALSE;
T_behind_full:
                await woken;
            end if;
        elsif Lent /\ at = 1 then
T_lend:
            \* The first datagram's buffer is lent to a send of the ring's.
            out := 1;
T_lent:
            either
                \* Its call waits for the send's end, and answers its verdict.
                await result[1] # "none";
            or
                \* Or its task goes away first: the send stays out, and still goes.
                orphaned := TRUE;
            end either;
            at := 2;
        elsif Queues then
T_behind:
            \* MUTATION behind: the call copies its datagram, queues it and answers taken at once,
            \* or answers the error the last queued datagram left, as a port's call does.
            if deferred # "none" then
                result[at] := deferred;
                deferred := "none";
            elsif Backend = "port" /\ Big(at) then
                \* A port refuses a datagram too large for the socket at once, as std's call does.
                result[at] := "err";
            else
                \* One datagram queued at a time: the call waits until the ring took it.
                await queued = 0;
                queued := at;
                result[at] := "ok";
            end if;
            at := at + 1;
        else
T_call:
            \* MUTATION direct_ahead: the call goes to the kernel with the side's send still out.
            if (out # 0 \/ pollFirst) /\ Mutation # "direct_ahead" then
                \* Something of the side is out first: the call waits for its end.
                if orphaned /\ out = 1 then
                    behindLent := TRUE;
                end if;
                woken := FALSE;
T_wait:
                await woken;
            elsif Big(at) then
                \* The kernel refuses it at once, before it looks for room.
                result[at] := "err";
                at := at + 1;
            elsif Len(socket) < Room then
                \* The kernel takes it at once.
                socket := Append(socket, at);
                result[at] := "ok";
                at := at + 1;
            else
                \* No room: the call waits for the poll, and then tries again.
                pollFirst := TRUE;
                again := TRUE;
                woken := FALSE;
T_room:
                await woken;
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

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* Every call that answered answered the kernel's own verdict on its datagram.
OwnResult == \A j \in 1..N : result[j] \in {"none", Verdict(j)}

\* Everything the kernel took, read or still held, it took in the order the task sent it.
Taken == wire \o socket
InOrder == \A a, b \in 1..Len(Taken) : a < b => Taken[a] < Taken[b]

\* No large chunk is ever copied: none is in the write-behind, or in the ring's send of it.
Held == behind \o sending
NoLargeCopy == \A i \in 1..Len(Held) : ~Large(Held[i])

\* Every call ends.
Termination == <>(pc["task"] = "Done")

\* Every datagram the kernel can take reaches the peer, the lent one too.
Delivered == <>(\A j \in 1..N : ~Big(j) => \E i \in 1..Len(wire) : wire[i] = j)

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A call found no room and waited for the poll.
NoAgain == ~again
\* A datagram too large for the kernel had its own call answer the refusal.
NoRefused == ~(\E j \in 1..N : Big(j) /\ result[j] = "err")
\* A call waited behind the lent send of a task that went.
NoBehindLent == ~behindLent
\* A large chunk waited for the write-behind ahead of it to go out.
NoWaitedSend == ~waitedSend
\* A small chunk was copied into the write-behind.
NoCopied == ~copied
=============================================================================

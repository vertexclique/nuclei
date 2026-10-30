----------------------------- MODULE RingSend -----------------------------
(***************************************************************************)
(* A datagram socket's sends on an io_uring ring (src/io/source/write.rs  *)
(* `WriteState::send_now`, the poll it waits on in `submit_ops` and its    *)
(* end in `op_done`, src/io/source/write/ops.rs), at the grain of a step   *)
(* under the side's guard. On epoll and kqueue the same call waits for the *)
(* socket's edge instead of the poll (`serve` wakes it): the poll's end    *)
(* here stands for either.                                                 *)
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
(* What must never happen: a call answers anything but the kernel's own    *)
(* verdict on its datagram (the bug the size sweep found: a datagram too   *)
(* large for the kernel was taken as sent, its error left for the next     *)
(* call); the task's datagrams reach the peer out of the order it sent     *)
(* them; a call waits forever.                                             *)
(*                                                                          *)
(* What is abstract: the side's guard (a step here runs whole under it;    *)
(* RingOp.tla checks the hand-off to the ring's thread), and a datagram's  *)
(* bytes: each is just its number.                                         *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    Lent,      \* TRUE: the first datagram is lent to a send of the ring's, and its task may go
    Mutation   \* "none", or the name of the one rule this run takes away

\* What each datagram is, in call order: "ok", or "big" (no socket takes it). A refused one
\* between two the kernel takes; with no lent send, one more, so a borrowed call meets a full
\* socket after one it filled itself.
Sends == IF Lent THEN <<"ok", "big", "ok">> ELSE <<"ok", "big", "ok", "ok">>
\* How many datagrams the socket holds.
Room == 1

\* How many datagrams the task sends.
N == Len(Sends)
\* Whether the kernel refuses datagram k whatever room there is.
Big(k) == Sends[k] = "big"
\* The kernel's own verdict on datagram k, which its call must answer.
Verdict(k) == IF Big(k) THEN "err" ELSE "ok"

(* --algorithm RingSend
variables
    socket = <<0>>,                    \* datagrams the socket holds for the peer, oldest first
    wire = <<>>,                       \* datagrams the peer read, in order
    out = 0,                            \* the side's operation with the ring: 0 none, -1 the poll, k datagram k's send
    pollFirst = FALSE,                 \* the side waits for a poll that says the socket has room
    woken = FALSE,                     \* the waiting call was woken
    queued = 0,                        \* (mutation behind) a datagram copied and queued for the ring
    deferred = "none",                 \* (mutation behind) an error kept for the next call
    result = [k \in 1..N |-> "none"],  \* what each call answered
    orphaned = FALSE,                  \* the lending task went before its send ended
    again = FALSE,                     \* (record) a call found no room and waited for the poll
    behindLent = FALSE;                \* (record) a call waited behind the lent send of a task that went

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
            await out \in 1..N /\ (Big(out) \/ Len(socket) < Room);
            if ~Big(out) then
                \* The kernel takes it: the socket holds it behind the others.
                socket := Append(socket, out);
            end if;
            if Lent /\ out = 1 then
                \* The lent send: its call gets the verdict, unless its task went.
                if ~orphaned then
                    result[1] := Verdict(1);
                end if;
            elsif Big(out) then
                \* (mutation behind) A queued datagram's error, kept for the next call.
                deferred := "err";
            end if;
            \* The send ended; every call waiting on the side is woken.
            out := 0;
            woken := TRUE;
        end either;
    end while;
end process;

\* The task: its calls, one after another.
fair process task = "task"
variables at = 1;   \* the datagram of the call under way
begin
T_loop:
    while at <= N do
        if Lent /\ at = 1 then
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
        elsif Mutation = "behind" then
T_behind:
            \* MUTATION behind: the call copies its datagram, queues it and answers taken at once,
            \* or answers the error the last queued datagram left.
            if deferred # "none" then
                result[at] := deferred;
                deferred := "none";
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
=============================================================================

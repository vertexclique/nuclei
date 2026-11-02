----------------------------- MODULE RingRecv -----------------------------
(***************************************************************************)
(* A datagram socket's receives (src/io/source/read/datagram.rs           *)
(* `receive_now` and `datagram_ready`, the bound in `read/ops.rs`          *)
(* `datagram_want`; the ring's poll armed in `submit_ops` and its arrivals *)
(* in `take_arrival` of read.rs, the edge in `serve`; on a port the        *)
(* receive into the pool, `receive` and `receive_starved` of read.rs, and  *)
(* `PortMem::RecvFrom` in port.rs), on epoll or kqueue, on an io_uring     *)
(* ring and on a Windows completion port, at the grain of a step under the *)
(* side's guard.                                                           *)
(*                                                                          *)
(* In plain words: a peer sends datagrams, and the socket holds them in    *)
(* order. A task receives them one call after another, into a buffer that  *)
(* holds every one of them, and before a receive it may first ask whether  *)
(* the socket is readable. A datagram is received whole into the task's    *)
(* own buffer by the task itself: on epoll and kqueue always, on a ring    *)
(* for a Unix datagram (which may be larger than any buffer of the pool).  *)
(* With nothing there the call waits for an edge, or for a poll of the     *)
(* ring, and tries again. The ring's poll stays armed once made: each      *)
(* datagram that comes says so, as an edge does, and the call that waits   *)
(* asks the kernel again while the poll stays. Only a ring's UDP socket    *)
(* has the ring receive ahead into a buffer of the pool, one that holds    *)
(* the largest datagram UDP carries, and the task takes from what came     *)
(* ahead, on a ring whose group holds any UDP datagram whole; elsewhere   *)
(* the task receives UDP itself too. A port receives UDP ahead too, into a *)
(* buffer of the pool that holds the largest datagram, whatever the call   *)
(* that waits asked for (a look at the sender asks for none); with no such *)
(* buffer in the pool it looks with a zero-byte receive that leaves the    *)
(* datagram queued, and the task receives it itself once the look said    *)
(* one came. Asking whether the socket is readable looks, and takes        *)
(* nothing.                                                                *)
(*                                                                          *)
(* What must never happen: a datagram cut although the task's buffer holds *)
(* it (the bug the size sweep found: a datagram read ahead into a buffer   *)
(* of the pool's largest class, smaller than it; and on a port, one        *)
(* received ahead into a buffer as small as the look that asked for it,    *)
(* whose cut came back as a success); datagrams received out               *)
(* of order; a call that waits forever.                                    *)
(*                                                                          *)
(* With `Buf` below the largest datagram the task's buffer is shorter than *)
(* one of them. A datagram longer than the buffer then answers as std's    *)
(* receive does on the same system (`AsStd`): on unix the bytes that fit,  *)
(* cut, as a success (the caller sizes the buffer); on Windows the bytes   *)
(* that fit and the error `WSAEMSGSIZE` (src/io/sys/windows.rs             *)
(* `recv_from_ready`, the lent receive's cut in port.rs, and the copy of a *)
(* datagram received ahead whole). Before, the port answered the cut as a  *)
(* success (`port_cut_ok`).                                                *)
(*                                                                          *)
(* What is abstract: the guard (a step runs whole under it; RingOp.tla      *)
(* checks the hand-off to the ring's thread), and sizes, counted in units: *)
(* the pool's largest buffer holds Largest units, its smallest Small.      *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    Backend,   \* "poller" (epoll, kqueue), "ring" (io_uring) or "port" (a Windows completion port)
    Unix,      \* TRUE: a Unix datagram socket, whose datagrams may outgrow the pool; FALSE: UDP
    Ahead,     \* TRUE: UDP is received ahead: a ring whose group holds any UDP datagram whole, a port
               \* the pool has a buffer for; FALSE on a port: the pool has none, so the port looks
    Buf,       \* the units the task's own buffer holds
    Mutation   \* "none", or the name of the one rule this run takes away

\* The backend the run is of.
Ring == Backend = "ring"
Port == Backend = "port"
\* The units the pool's largest buffer holds.
Largest == 2
\* The units its smallest buffer holds (512 B, less than a datagram of the largest size).
Small == 1
\* The datagrams the peer sends, by size in order: a small one, then the largest the socket
\* carries, which for a Unix socket is more than the pool's largest buffer holds.
Sizes == IF Unix THEN <<1, 3>> ELSE <<1, Largest>>
\* How many datagrams there are.
N == Len(Sizes)
\* Whether the task receives straight into its own buffer, or takes what the ring read ahead:
\* only a ring's UDP side receives ahead, and only on a ring that does (`datagrams_ahead`).
Direct == Backend = "poller" \/ Unix \/ ~Ahead
\* The buffer of the pool a ring's UDP receive ahead takes: one that holds the largest datagram.
\* MUTATION bound_small: a buffer bound below the largest datagram UDP carries.
Bound == IF Mutation = "bound_small" THEN Largest - 1 ELSE Largest
\* The smaller of two sizes.
Min(a, b) == IF a < b THEN a ELSE b
\* What a receive of datagram `n`, `kept` of its units at hand, answers into the task's buffer: the
\* units that fit, and whether it is an error: on a port, a datagram longer than the buffer
\* (`WSAEMSGSIZE`), as std's receive answers it there; on unix a cut, as a success.
\* MUTATION port_cut_ok: the port answers the cut as a success (the code before).
Answer(n, kept) == <<n, Min(kept, Buf), Port /\ Sizes[n] > Buf /\ Mutation # "port_cut_ok">>

(* --algorithm RingRecv
variables
    sent = 0,              \* datagrams the peer sent so far
    socket = <<>>,         \* datagrams the socket holds, oldest first, by number
    ahead = <<>>,          \* datagrams received ahead into the pool, as <<number, units kept>>
    out = "none",          \* the ring's operation for the side: "none", "recv" or "poll" (armed)
    edge = FALSE,          \* the armed poll has a datagram's arrival to tell the side
    waits = FALSE,         \* the task waits for an edge, the ring's poll or what comes ahead
    asked = "recv",        \* what the call that waits asked for: a receive, or a look ("look")
    cap = Largest,         \* the units the buffer of the receive out holds
    readable = FALSE,      \* a port's zero-byte look said a datagram came, and none was taken since
    woken = FALSE,         \* the waiting task was woken
    got = <<>>,            \* what the task received, as <<number, units, an error>>
    waited = FALSE;        \* (record) a call waited for an edge or a poll

\* The peer sends its datagrams, one after another.
fair process peer = "peer"
begin
P_send:
    while sent < N do
        \* One more datagram reaches the socket.
        sent := sent + 1;
        socket := Append(socket, sent);
        \* On epoll and kqueue it brings an edge, which wakes a task that waits.
        \* MUTATION no_wake: the edge wakes nobody.
        if Backend = "poller" /\ waits /\ Mutation # "no_wake" then
            woken := TRUE;
        end if;
        \* On a ring the armed poll sees it, and has it to tell the side.
        if Ring /\ out = "poll" then
            edge := TRUE;
        end if;
    end while;
end process;

\* The ring or the port: it gives the side the operation a waiting task needs, and applies each end.
fair process ring \in (IF Backend = "poller" THEN {} ELSE {"ring"})
begin
R_loop:
    while TRUE do
        either
            \* A task waits and nothing is out: a side the ring receives ahead has a datagram
            \* received ahead, one whose task receives itself a poll that says one came.
            \* MUTATION ahead_after_poll: a side whose task receives itself gets a receive ahead
            \* into the smallest buffer.
            await waits /\ out = "none";
            if Direct /\ Mutation # "ahead_after_poll" then
                if Port then
                    \* A port looks with a zero-byte receive, MSG_PEEK, which takes nothing.
                    out := "look";
                else
                    \* The poll is armed, and at once has a datagram to tell if one is there.
                    out := "poll";
                    edge := socket # <<>>;
                end if;
            else
                out := "recv";
                \* The buffer of the pool it takes holds the largest datagram (`datagram_want`).
                \* MUTATION task_sized: one as large as the call that waits asked for, as it was: a
                \* look at the sender asks for none, and gets the pool's smallest buffer.
                cap := IF Mutation = "task_sized" /\ asked = "look" THEN Small ELSE Bound;
            end if;
        or
            \* A receive ahead ends once a datagram came: what its buffer holds is kept, a cut one
            \* as a success (a port's STATUS_BUFFER_OVERFLOW, as unix's MSG_TRUNC).
            await out = "recv" /\ socket # <<>>;
            ahead := Append(ahead, <<Head(socket),
                Min(Sizes[Head(socket)], IF Unix THEN 1 ELSE cap)>>);
            socket := Tail(socket);
            out := "none";
            woken := TRUE;
        or
            \* A port's look ends once a datagram came, and leaves it queued: the task receives it.
            await out = "look" /\ socket # <<>>;
            out := "none";
            readable := TRUE;
            woken := TRUE;
        or
            \* The armed poll tells the side a datagram came, and stays armed; it takes nothing.
            \* MUTATION no_wake: its word wakes nobody.
            await edge;
            edge := FALSE;
            if waits /\ Mutation # "no_wake" then
                woken := TRUE;
            end if;
        end either;
    end while;
end process;

\* The task: for each datagram, maybe a look at whether the socket is readable, then a receive.
fair process task = "task"
variables at = 1;    \* the datagram the next receive is for
begin
T_loop:
    while at <= N do
        either
            skip;
        or
T_look:
            \* Is the socket readable? A side the ring receives ahead has it once a datagram came.
            \* MUTATION ahead_largest: every side looks by receiving ahead into the pool's largest
            \* buffer, as it did before.
            if ~Direct \/ Mutation = "ahead_largest" then
                if ahead # <<>> then
                    skip;
                elsif Backend = "poller" /\ socket # <<>> then
                    \* epoll and kqueue receive ahead at once.
                    ahead := Append(ahead, <<Head(socket), Min(Sizes[Head(socket)], Largest)>>);
                    socket := Tail(socket);
                else
                    waits := TRUE;
                    woken := FALSE;
                    asked := "look";
T_look_wait:
                    await woken;
                    waits := FALSE;
                    goto T_look;
                end if;
            elsif socket = <<>> \/ (Port /\ ~readable) then
                \* It looks without taking: with nothing there it waits, and looks again (on a
                \* port, until its zero-byte look said a datagram came).
                waits := TRUE;
                woken := FALSE;
T_peek_wait:
                await woken;
                waits := FALSE;
                goto T_look;
            end if;
        end either;
T_recv:
        if ahead # <<>> then
            \* What came ahead comes first.
            got := Append(got, Answer(Head(ahead)[1], Head(ahead)[2]));
            ahead := Tail(ahead);
            at := at + 1;
        elsif Direct /\ socket # <<>> /\ ~(Mutation = "armed_waits" /\ out = "poll")
              /\ (readable \/ ~Port) then
            \* The task's own receive takes the oldest datagram whole, the armed poll staying (on
            \* a port once its look said one came, `receive_starved`; the next waits for another).
            \* MUTATION armed_waits: the armed poll counts as an operation out, which the call
            \* waits behind, so it never asks the kernel again.
            got := Append(got, Answer(Head(socket), Sizes[Head(socket)]));
            socket := Tail(socket);
            readable := FALSE;
            at := at + 1;
        else
            \* Nothing there: the call waits for an edge, the poll or a receive ahead.
            waits := TRUE;
            woken := FALSE;
            asked := "recv";
            waited := TRUE;
T_wait:
            await woken;
            waits := FALSE;
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

\* Every datagram received came whole: the task's buffer holds each of them.
NoCut == \A j \in 1..Len(got) : got[j][2] = Sizes[got[j][1]]

\* The datagrams are received in the order they were sent.
InOrder == \A j \in 1..Len(got) : got[j][1] = j

\* Every receive answers as std's does on the same system: a datagram that fits the task's buffer
\* whole and as a success; a longer one cut to it, as a success on unix, with `WSAEMSGSIZE` on Windows.
AsStd == \A j \in 1..Len(got) :
    LET n == got[j][1] IN
    IF Sizes[n] <= Buf THEN got[j][2] = Sizes[n] /\ ~got[j][3]
    ELSE got[j][2] = Buf /\ got[j][3] = Port

\* Every call ends: the task receives every datagram.
Termination == <>(pc["task"] = "Done")

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A call waited for an edge, the ring's poll or a receive ahead.
NoWait == ~waited
\* A datagram the ring received ahead was taken by a receive.
NoAheadTaken == Len(got) = 0 \/ Direct
\* A port's zero-byte look said a datagram came, for the task to receive itself.
NoLook == ~readable
=============================================================================

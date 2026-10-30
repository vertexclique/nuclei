----------------------------- MODULE RingRecv -----------------------------
(***************************************************************************)
(* A datagram socket's receives (src/io/source/read.rs `receive_now`,      *)
(* `datagram_ready` and the bound in `read/ops.rs` `datagram_want`; the    *)
(* poll's end in `op_done`, the edge in `serve`), on epoll or kqueue and on *)
(* an io_uring ring, at the grain of a step under the side's guard.        *)
(*                                                                          *)
(* In plain words: a peer sends datagrams, and the socket holds them in    *)
(* order. A task receives them one call after another, into a buffer that  *)
(* holds every one of them, and before a receive it may first ask whether  *)
(* the socket is readable. A datagram is received whole into the task's    *)
(* own buffer by the task itself: on epoll and kqueue always, on a ring    *)
(* for a Unix datagram (which may be larger than any buffer of the pool).  *)
(* With nothing there the call waits for an edge, or for a poll of the     *)
(* ring, and tries again. Only a ring's UDP socket has the ring receive    *)
(* ahead into a buffer of the pool, one that holds the largest datagram    *)
(* UDP carries, and the task takes from what came ahead. Asking whether    *)
(* the socket is readable looks, and takes nothing.                        *)
(*                                                                          *)
(* What must never happen: a datagram cut although the task's buffer holds *)
(* it (the bug the size sweep found: a datagram read ahead into a buffer   *)
(* of the pool's largest class, smaller than it); datagrams received out   *)
(* of order; a call that waits forever.                                    *)
(*                                                                          *)
(* What is abstract: the guard (a step runs whole under it; RingOp.tla      *)
(* checks the hand-off to the ring's thread), and sizes, counted in units: *)
(* the pool's largest buffer holds Largest units.                          *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    Ring,      \* TRUE: an io_uring ring; FALSE: epoll or kqueue
    Unix,      \* TRUE: a Unix datagram socket, whose datagrams may outgrow the pool; FALSE: UDP
    Mutation   \* "none", or the name of the one rule this run takes away

\* The units the pool's largest buffer holds.
Largest == 2
\* The datagrams the peer sends, by size in order: a small one, then the largest the socket
\* carries, which for a Unix socket is more than the pool's largest buffer holds.
Sizes == IF Unix THEN <<1, 3>> ELSE <<1, Largest>>
\* How many datagrams there are.
N == Len(Sizes)
\* Whether the task receives straight into its own buffer, or takes what the ring read ahead.
Direct == ~Ring \/ Unix
\* The buffer of the pool a ring's UDP receive ahead takes: one that holds the largest datagram.
\* MUTATION bound_small: a buffer bound below the largest datagram UDP carries.
Bound == IF Mutation = "bound_small" THEN Largest - 1 ELSE Largest
\* The smaller of two sizes.
Min(a, b) == IF a < b THEN a ELSE b

(* --algorithm RingRecv
variables
    sent = 0,              \* datagrams the peer sent so far
    socket = <<>>,         \* datagrams the socket holds, oldest first, by number
    ahead = <<>>,          \* datagrams received ahead into the pool, as <<number, units kept>>
    out = "none",          \* the ring's operation for the side: "none", "recv" or "poll"
    waits = FALSE,         \* the task waits for an edge, the ring's poll or what comes ahead
    woken = FALSE,         \* the waiting task was woken
    got = <<>>,            \* what the task received, as <<number, units>>
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
        if ~Ring /\ waits /\ Mutation # "no_wake" then
            woken := TRUE;
        end if;
    end while;
end process;

\* The ring: it gives the side the operation a waiting task needs, and applies each end.
fair process ring \in (IF Ring THEN {"ring"} ELSE {})
begin
R_loop:
    while TRUE do
        either
            \* A task waits and nothing is out: a UDP side has a datagram received ahead, a Unix
            \* side a poll that says one came.
            \* MUTATION ahead_after_poll: a Unix side gets a receive ahead into the smallest buffer.
            await waits /\ out = "none";
            if Unix /\ Mutation # "ahead_after_poll" then
                out := "poll";
            else
                out := "recv";
            end if;
        or
            \* A receive ahead ends once a datagram came: what its buffer holds is kept.
            await out = "recv" /\ socket # <<>>;
            ahead := Append(ahead, <<Head(socket),
                Min(Sizes[Head(socket)], IF Unix THEN 1 ELSE Bound)>>);
            socket := Tail(socket);
            out := "none";
            woken := TRUE;
        or
            \* The poll ends once a datagram came; it takes nothing.
            \* MUTATION no_wake: the poll's end wakes nobody.
            await out = "poll" /\ socket # <<>>;
            out := "none";
            if Mutation # "no_wake" then
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
            \* Is the socket readable? A UDP side on a ring has it once a datagram came ahead.
            \* MUTATION ahead_largest: every side looks by receiving ahead into the pool's largest
            \* buffer, as it did before.
            if (Ring /\ ~Unix) \/ Mutation = "ahead_largest" then
                if ahead # <<>> then
                    skip;
                elsif ~Ring /\ socket # <<>> then
                    \* epoll and kqueue receive ahead at once.
                    ahead := Append(ahead, <<Head(socket), Min(Sizes[Head(socket)], Largest)>>);
                    socket := Tail(socket);
                else
                    waits := TRUE;
                    woken := FALSE;
T_look_wait:
                    await woken;
                    waits := FALSE;
                    goto T_look;
                end if;
            elsif socket = <<>> then
                \* It looks without taking: with nothing there it waits, and looks again.
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
            got := Append(got, Head(ahead));
            ahead := Tail(ahead);
            at := at + 1;
        elsif Direct /\ socket # <<>> then
            \* The task's own receive takes the oldest datagram whole.
            got := Append(got, <<Head(socket), Sizes[Head(socket)]>>);
            socket := Tail(socket);
            at := at + 1;
        else
            \* Nothing there: the call waits for an edge, the poll or a receive ahead.
            waits := TRUE;
            woken := FALSE;
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

\* Every call ends: the task receives every datagram.
Termination == <>(pc["task"] = "Done")

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A call waited for an edge, the ring's poll or a receive ahead.
NoWait == ~waited
\* A datagram the ring received ahead was taken by a receive.
NoAheadTaken == Len(got) = 0 \/ ~Ring \/ Unix
=============================================================================

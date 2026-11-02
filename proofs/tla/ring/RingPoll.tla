------------------------------ MODULE RingPoll ------------------------------
(***************************************************************************)
(* A stream's reads on an io_uring ring that go by readiness: the ring's   *)
(* multishot poll says when the socket may hold bytes, and the task reads  *)
(* them straight into its own buffer, as on epoll; and the switch between   *)
(* that and the multishot receive, whose chunks the side reads ahead       *)
(* (src/io/source/read/direct.rs `read_direct`, `poll_first`, and the      *)
(* switch's `runs_ahead` and `count_fit` (from `take_arrival`);          *)
(* src/io/source/read/ops.rs `submit_ops`, `op_done`; src/io/source/read.rs *)
(* `take_arrival`), at the grain of the kernel's completions, the ring     *)
(* thread's reaping and the reader's polls.                                *)
(*                                                                          *)
(* Who acts. A peer writes messages. The kernel holds them in the socket,  *)
(* posts a completion each time bytes come while the side's poll is armed, *)
(* and, with the multishot receive armed instead, takes the socket's       *)
(* messages into chunks, one completion each. A cancel ends the operation  *)
(* it names, and the kernel may end a poll on its own too. The ring's      *)
(* thread reaps the completions in order. The reader reads: by readiness,  *)
(* straight from the socket, or the receive's chunks in the order they     *)
(* came; and it may switch from one way to the other at any poll.          *)
(*                                                                          *)
(* What can happen. A read by readiness takes up to `Buf` messages. With   *)
(* `Exact` the kernel says how many it left (`TCP_INQ`): none, and the     *)
(* next read waits for the poll; without, a read that took less than it    *)
(* asked for emptied the socket. A read that finds nothing waits for the   *)
(* poll too. The poll is armed only when the side waits, and the kernel    *)
(* looks at the socket as it arms it, so bytes already there say so at     *)
(* once. A poll that ends (a cancel, or the kernel's own end) is armed     *)
(* again by the next wait. A switch to the receive cancels the poll and    *)
(* arms the receive once the poll ended; a switch back cancels the         *)
(* receive, and reads from the socket again only once every chunk it       *)
(* brought was read and its end came.                                      *)
(*                                                                          *)
(* With `Close` the peer closes the socket after its messages, and the     *)
(* kernel's count leaves the pending end out, as a Unix stream's does: the *)
(* poll reports the hangup, which marks the side unsure (`Arrival::Hangup`,*)
(* `short_reads_unsure`), and an unsure side reads again on a count of     *)
(* none, finding the end. A side that does not mark it waits for a poll    *)
(* that said everything already.                                           *)
(*                                                                          *)
(* What must never happen. A message is lost, doubled or read out of       *)
(* order. The side has a poll and a receive in the kernel at once. The     *)
(* reader waits for a poll that will never say anything while messages    *)
(* sit in the socket.                                                      *)
(*                                                                          *)
(* What is abstract. A message is one unit; the side's guard (a poll runs  *)
(* whole under it: the side family checks it); a cancel takes the          *)
(* operation at once once the kernel acts; completions the kernel posts    *)
(* for a poll while one of its completions waits to be reaped are one      *)
(* (the reader reads until the socket is empty on it); the thresholds that *)
(* choose a switch (any poll may switch, at most `MaxSwitch` times: the   *)
(* code switches to the receive on a count that leaves more than another  *)
(* read takes, and back after four arrivals that each came to a reader    *)
(* waiting with nothing read ahead and fit its read).                     *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC                                             \* numbers, sequences, TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    N,          \* messages the peer writes
    Buf,        \* messages a read by readiness asks for
    Exact,      \* TRUE: the kernel counts what a read leaves (TCP_INQ); FALSE: a read goes by its length
    Ends,       \* times the kernel may end the side's poll on its own
    Switch,     \* TRUE: the side may switch between readiness and the multishot receive
    MaxSwitch,  \* switches at most, so a side that keeps switching cannot spin the model
    Close,      \* TRUE: the peer closes after its messages, and the count leaves the end out (no switch then)
    Mutation    \* "none", or the name of the one rule this run takes away

ASSUME Close => ~Switch                                                      \* the end through the receive is RingEnd.tla's

\* The smaller of two numbers.
Min(a, b) == IF a < b THEN a ELSE b                                          \* the lesser
\* Whether a readiness completion waits in the queue `q`.
ReadyIn(q) == \E i \in DOMAIN q : q[i].k = "ready"                           \* one of them says ready
\* Whether a readiness completion that reports the hangup waits in the queue `q`.
HupIn(q) == \E i \in DOMAIN q : q[i].k = "ready" /\ q[i].hup                 \* one of them says hangup

(* --algorithm RingPoll                                                        \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    sock = <<>>,          \* messages in the socket, by id, in order
    written = 0,          \* messages the peer wrote
    closed = FALSE,       \* the peer closed the socket
    unsure = FALSE,       \* the side was told of a hangup: a count of none no longer says the socket is empty
    sawEnd = FALSE,       \* the reader read the end
    kpoll = FALSE,        \* the side's multishot poll is in the kernel
    krecv = FALSE,        \* the side's multishot receive is in the kernel
    cancelPoll = FALSE,   \* a cancel of the poll is on its way
    cancelRecv = FALSE,   \* a cancel of the receive is on its way
    kends = 0,            \* polls the kernel ended on its own
    cq = <<>>,            \* completions not reaped yet
    out = "none",         \* the side's operation out, as the side sees it: "none", "poll", "recv"
    mode = "ready",       \* how the side reads: "ready" (straight from the socket) or "recv" (the chunks)
    pollFirst = FALSE,    \* the side found the socket empty: the next read waits for the poll
    ahead = <<>>,         \* messages the receive brought, not read yet
    got = <<>>,           \* what the reader got, in order
    switches = 0,         \* switches made
    armedOnce = FALSE,    \* (record) a poll was armed
    rearmed = FALSE,      \* (record) a poll was armed again after one ended
    toRecv = FALSE,       \* (record) the side switched to the receive
    toReady = FALSE;      \* (record) the side switched back to readiness

define                                                                       \* names the code and the properties use:
    PeerDone == written = N /\ (closed \/ ~Close)                            \* the peer wrote everything, and closed
    ReaderDone == Len(got) = N /\ (sawEnd \/ ~Close)                         \* the reader has every message, and the end
end define;                                                                  \* end of the names

\* The peer: writes its messages, each one saying so to an armed poll.
fair process peer = "peer"                                                   \* weakly fair, like every process
begin                                                                        \* the peer's steps:
P_write:                                                                     \* one message
    while written < N do                                                     \* while it has messages
        written := written + 1;                                              \* the next
        sock := Append(sock, written);                                       \* into the socket
        if kpoll /\ ~ReadyIn(cq) then                                        \* an armed poll with no readiness pending:
            cq := Append(cq, [k |-> "ready", hup |-> FALSE]);                \* it says the socket may be readable
        end if;                                                              \* (one pending stands for every edge)
    end while;                                                               \* all written
P_close:                                                                     \* the end
    if Close then                                                            \* a peer that closes:
        closed := TRUE;                                                      \* the socket's end is after the messages
        if kpoll /\ ~HupIn(cq) then                                          \* an armed poll reports the hangup
            cq := Append(cq, [k |-> "ready", hup |-> TRUE]);                 \* with its own completion
        end if;                                                              \* the hangup is out
    end if;                                                                  \* the peer is done
end process;                                                                 \* end of the peer

\* The kernel: the receive's arrivals, and the ends of the side's operations.
fair process kernel = "kernel"                                               \* weakly fair, like every process
begin                                                                        \* the kernel's steps:
K_loop:                                                                      \* one completion
    while ~(ReaderDone /\ cq = <<>> /\ ~kpoll /\ ~krecv) do                  \* while anything is left to end
        either                                                               \* the receive takes the oldest message:
            await krecv /\ sock # <<>>;                                      \* armed, with a message there
            cq := Append(cq, [k |-> "chunk", m |-> Head(sock)]);             \* a chunk of it
            sock := Tail(sock);                                              \* out of the socket
        or                                                                   \* a cancel takes the receive:
            await krecv /\ cancelRecv;                                       \* asked for
            krecv := FALSE || cancelRecv := FALSE;                           \* it is over
            cq := Append(cq, [k |-> "recvend"]);                             \* its last completion, after its chunks
        or                                                                   \* a cancel takes the poll:
            await kpoll /\ cancelPoll;                                       \* asked for
            kpoll := FALSE || cancelPoll := FALSE;                           \* it is over
            cq := Append(cq, [k |-> "pollend"]);                             \* its last completion
        or                                                                   \* the kernel ends the poll on its own:
            await kpoll /\ ~cancelPoll /\ kends < Ends;                      \* at most `Ends` times
            kpoll := FALSE;                                                  \* it is over
            kends := kends + 1;                                              \* counted
            cq := Append(cq, [k |-> "pollend"]);                             \* its last completion, with no MORE
        end either;                                                          \* the next
    end while;                                                               \* the reader has all
end process;                                                                 \* end of the kernel

\* The ring's thread: applies each completion, in the order it came.
fair process ring = "ring"                                                   \* weakly fair, like every process
variables c = [k |-> "none"];                                                \* the completion reaped
begin                                                                        \* the ring thread's steps:
R_loop:                                                                      \* one completion
    while ~(ReaderDone /\ cq = <<>> /\ ~kpoll /\ ~krecv) do                  \* while any may come
        await cq # <<>>                                                      \* one is there,
              \/ (ReaderDone /\ ((kpoll /\ ~cancelPoll) \/ (krecv /\ ~cancelRecv)));   \* or a done side's op to let go
        if cq # <<>> then                                                    \* one is there:
            c := Head(cq);                                                   \* the oldest
            cq := Tail(cq);                                                  \* is reaped
            if c.k = "ready" then                                            \* the poll said readable (`take_arrival`):
                pollFirst := FALSE;                                          \* the next read goes to the socket
                \* MUTATION hangup_ignored: the hangup the poll reports does not mark the side unsure.
                if c.hup /\ Mutation # "hangup_ignored" then                 \* a hangup reported:
                    unsure := TRUE;                                          \* a count of none is not trusted again
                end if;                                                      \* the report is taken
            elsif c.k = "chunk" then                                         \* a chunk of the receive:
                ahead := Append(ahead, c.m);                                 \* read ahead, in order
            elsif c.k = "recvend" then                                       \* the receive's end (`op_done`):
                \* MUTATION edge_in_switch: past a switch back to readiness, the receive's end is taken as
                \* the socket found empty, its poll as one still armed: the side waits for a poll it has not.
                if Mutation = "edge_in_switch" /\ mode = "ready" then        \* (mutated)
                    out := "poll" || pollFirst := TRUE;                      \* the stale poll and the empty socket
                else                                                         \* else
                    out := "none";                                           \* nothing out
                end if;                                                      \* the end is applied
            else                                                             \* the poll's end (`op_done`):
                if out = "poll" then out := "none"; end if;                  \* nothing out; the next wait arms one
            end if;                                                          \* the completion is handled
        else                                                                 \* the reader is done:
            if kpoll /\ ~cancelPoll then cancelPoll := TRUE; end if;         \* the side lets its poll go,
            if krecv /\ ~cancelRecv then cancelRecv := TRUE; end if;         \* and its receive, as it closes
        end if;                                                              \* the step is over
    end while;                                                               \* nothing more will come
end process;                                                                 \* end of the ring's thread

\* The reader: each poll reads what it can, or arms what it waits on, or switches.
fair process reader = "reader"                                               \* weakly fair, like every process
begin                                                                        \* the reader's steps:
D_loop:                                                                      \* a poll
    while ~ReaderDone do                                                     \* while messages may come
        either                                                               \* the oldest message the receive brought:
            await ahead # <<>>;                                              \* one is read ahead
            got := Append(got, Head(ahead));                                 \* read
            ahead := Tail(ahead);                                            \* taken
        or                                                                   \* a read by readiness, straight from the socket:
            \* MUTATION direct_before_ahead: a read from the socket while chunks wait to be read or the receive is out.
            await mode = "ready" /\ ~pollFirst                               \* readiness, the socket not found empty,
                  /\ ((ahead = <<>> /\ out # "recv") \/ Mutation = "direct_before_ahead");   \* nothing of the receive's first
            with n = Min(Buf, Len(sock)), rest = Len(sock) - Min(Buf, Len(sock)) do   \* what it takes, and leaves
                got := got \o SubSeq(sock, 1, n);                            \* the messages, in order
                sock := SubSeq(sock, n + 1, Len(sock));                      \* out of the socket
                \* MUTATION drained_after_fire: a read that took all it asked for is taken as having emptied the
                \* socket, whatever the kernel says it left: the poll already fired for those bytes.
                if n = 0 /\ closed then                                      \* nothing left, and the peer closed:
                    sawEnd := TRUE;                                          \* the read finds the end
                elsif n = 0 then                                             \* the socket was empty:
                    pollFirst := TRUE;                                       \* wait for the poll
                elsif Mutation = "drained_after_fire" /\ n = Buf then        \* (mutated) a full read:
                    pollFirst := TRUE;                                       \* taken as emptying it
                elsif Exact then                                             \* the kernel counted what is left:
                    pollFirst := rest = 0 /\ ~unsure;                        \* none, and the next read waits, unless unsure
                else                                                         \* else by the read's length:
                    pollFirst := n < Buf;                                    \* short, and the socket is empty
                end if;                                                      \* the next read knows
            end with;                                                        \* the read is over
        or                                                                   \* the poll armed, for a side that waits on it:
            \* MUTATION no_rearm: a poll is armed once, and never again after it ended.
            await mode = "ready" /\ pollFirst /\ out = "none" /\ ahead = <<>>   \* readiness, found empty, nothing out
                  /\ (~armedOnce \/ Mutation # "no_rearm");                  \* (mutated) once only
            out := "poll";                                                   \* out
            kpoll := TRUE;                                                   \* in the kernel
            rearmed := rearmed \/ armedOnce;                                 \* (record) armed again
            armedOnce := TRUE;                                               \* (record) armed
            if (sock # <<>> \/ closed) /\ ~ReadyIn(cq) then                  \* the kernel looks as it arms it:
                cq := Append(cq, [k |-> "ready", hup |-> closed]);           \* bytes or the end there say so at once
            end if;                                                          \* the poll is armed
        or                                                                   \* the receive armed, for a side that reads its chunks:
            await mode = "recv" /\ out = "none" /\ ahead = <<>>;             \* receive mode, nothing out or ahead
            out := "recv";                                                   \* out
            krecv := TRUE;                                                   \* in the kernel
        or                                                                   \* a switch to the multishot receive:
            await Switch /\ mode = "ready" /\ switches < MaxSwitch;          \* allowed, and not too often
            mode := "recv";                                                  \* reads take chunks from now on
            switches := switches + 1;                                        \* counted
            toRecv := TRUE;                                                  \* (record)
            if out = "poll" /\ kpoll then cancelPoll := TRUE; end if;        \* the poll out is cancelled
        or                                                                   \* a switch back to readiness:
            await Switch /\ mode = "recv" /\ switches < MaxSwitch;           \* allowed, and not too often
            mode := "ready";                                                 \* reads go to the socket from now on
            switches := switches + 1;                                        \* counted
            toReady := TRUE;                                                 \* (record)
            pollFirst := FALSE;                                              \* the first read looks at the socket
            if out = "recv" /\ krecv then cancelRecv := TRUE; end if;        \* the receive out is cancelled
        end either;                                                          \* the poll is over
    end while;                                                               \* the reader has all
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

\* The reader gets the messages in order, none lost, none twice.
InOrder == got = [i \in 1..Len(got) |-> i]                                   \* the first so many, in order

\* The side never has a poll and a receive in the kernel at once.
OneOut == ~(kpoll /\ krecv)                                                  \* never both

\* `Termination` (every process ends, so the reader got every message) is the one the translator writes.

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A poll is armed again after one ended.
NoRearm == ~rearmed                                                          \* never armed twice
\* The side switches to the receive and back to readiness.
NoSwitchBack == ~(toRecv /\ toReady)                                         \* never both ways
=============================================================================

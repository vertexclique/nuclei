----------------------------- MODULE RingStarve -----------------------------
(***************************************************************************)
(* Stream reads and replies on an io_uring ring that share one shard's     *)
(* pool of IO buffers, of sockets or pipes (src/io/source/read/ops.rs      *)
(* `submit_ops` and `op_done`; the reader's own call in                    *)
(* src/io/source/read.rs `read_direct`, `reads_itself` and `reads_bytes`;  *)
(* the writer's in src/io/source/write.rs `write`, `write_direct`,         *)
(* `writes_itself` and `writes_bytes`; `buffer` and `unstarve` of each),   *)
(* at the grain of a step under each side's guard. A ring write goes       *)
(* straight from the caller's buffer at every size (`WRITE_RING` of        *)
(* src/io/source/direct.rs is 0): the starved writer's step is that one,   *)
(* now also taken with the pool full.                                      *)
(*                                                                         *)
(* In plain words: several connections each have a task that reads one     *)
(* request and writes one reply. With no receive group to draw from (none  *)
(* configured, or the group ran dry), a read takes a buffer of the pool    *)
(* for a receive that waits in the kernel until the peer sends, holding    *)
(* that buffer all the while. Some peers never send. When the pool has no  *)
(* buffer left, a reader is starved: it asks the ring for a poll, which    *)
(* holds no buffer, and once the poll says bytes came it reads them into   *)
(* its own buffer. A reply is copied into a buffer of the pool that the    *)
(* ring sends out and gives back; a writer the pool has none for waits for *)
(* a poll that says the socket takes bytes, and writes the reply from its  *)
(* own buffer. A buffer that comes back wakes every starved side to try    *)
(* the pool again.                                                         *)
(*                                                                         *)
(* What must never happen: a peer's request that is never read, or never   *)
(* answered, because every buffer of the pool waits in the kernel for      *)
(* peers that never send (the stall the vs-tokio benchmark found: 4096     *)
(* connections of 16 KiB requests, the default cap, left requests          *)
(* unanswered for good).                                                   *)
(*                                                                         *)
(* What is abstract: the guards (each step runs whole under its side's),   *)
(* the ring's thread (each completion is applied as the kernel ends it),   *)
(* and the bytes: one request and one reply per connection, each whole.    *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Idle,      \* how many of the connections have peers that never send
    Mutation   \* "none", or the name of the one rule this run takes away

\* The connections, and the buffers the pool holds: fewer than the connections.
Conns == 1..3
Pool == 2
\* The connections whose peers send their request; the first `Idle` never do.
Active == {c \in Conns : c > Idle}

(* --algorithm RingStarve
variables
    free = Pool,                        \* buffers of the pool no one holds
    sock = [c \in Conns |-> 0],         \* a request in the socket, not received yet
    op = [c \in Conns |-> "none"],      \* the side's operation: "none", "recv" (holds a buffer), "poll"
    ahead = [c \in Conns |-> FALSE],    \* a request received into a buffer of the pool
    starved = [c \in Conns |-> FALSE],  \* the pool had no buffer for the side
    woken = [c \in Conns |-> FALSE],    \* the waiting reader was woken
    got = [c \in Conns |-> FALSE],      \* the reader has its request
    sending = [c \in Conns |-> FALSE],  \* a reply copied into a buffer of the pool, not sent yet
    wstarved = [c \in Conns |-> FALSE], \* the pool had no buffer for the reply
    wpoll = [c \in Conns |-> FALSE],    \* the writer's poll for room is out
    replied = [c \in Conns |-> FALSE],  \* the reply went out
    direct = FALSE,                     \* (record) a starved reader read into its own buffer
    starvedOnce = FALSE;                \* (record) a reader was starved

\* The peers that send: each request reaches its socket, once.
fair process peer = 0
variables left = Active;    \* the peers that have not sent yet
begin
P_send:
    while left # {} do
        with c \in left do
            \* The request is in the socket now.
            sock[c] := 1;
            left := left \ {c};
        end with;
    end while;
end process;

\* The kernel ends the operations out, and the ring applies each end at once.
fair process kernel = 4
begin
K_loop:
    while TRUE do
        either
            \* A reply in a buffer of the pool goes out, and the buffer comes back: every starved
            \* side is woken to try the pool again.
            with c \in {d \in Conns : sending[d]} do
                sending[c] := FALSE;
                replied[c] := TRUE;
                free := free + 1;
                woken := [d \in Conns |-> IF starved[d] \/ wstarved[d] THEN TRUE ELSE woken[d]];
                starved := [d \in Conns |-> FALSE];
                wstarved := [d \in Conns |-> FALSE];
            end with;
        or
            \* A writer's poll ends: the socket takes bytes (its peer reads), and the writer is woken.
            with c \in {d \in Conns : wpoll[d]} do
                wpoll[c] := FALSE;
                woken[c] := TRUE;
            end with;
        or
        with c \in {d \in Conns : op[d] # "none" /\ sock[d] = 1} do
            if op[c] = "recv" then
                \* The receive takes the request into its buffer; the reader is woken.
                ahead[c] := TRUE;
                sock[c] := 0;
                woken[c] := TRUE;
            \* MUTATION no_wake: the poll's end wakes nobody.
            elsif Mutation # "no_wake" then
                \* The poll says the request came, and takes nothing; the reader is woken.
                woken[c] := TRUE;
            end if;
            \* The operation ended.
            op[c] := "none";
        end with;
        end either;
    end while;
end process;

\* Each connection's reader: its read, polled again each time it is woken, until it has its
\* request.
fair process reader \in Conns
begin
R_loop:
    while ~got[self] do
R_poll:
        if ahead[self] then
            \* The request came into a buffer of the pool: the reader takes it, and the buffer
            \* goes back, which wakes every starved side to try the pool again.
            got[self] := TRUE;
            ahead[self] := FALSE;
            free := free + 1;
            woken := [c \in Conns |-> IF starved[c] \/ wstarved[c] THEN TRUE ELSE woken[c]];
            starved := [c \in Conns |-> FALSE];
            wstarved := [c \in Conns |-> FALSE];
        \* MUTATION park: a starved reader waits for a buffer to come back, and nothing else.
        elsif starved[self] /\ op[self] = "none" /\ Mutation # "park" then
            if sock[self] = 1 then
                \* The poll said bytes came: the reader reads them into its own buffer.
                got[self] := TRUE;
                sock[self] := 0;
                direct := TRUE;
            else
                \* Nothing yet: a poll that holds no buffer, and the reader waits for it.
                op[self] := "poll";
                woken[self] := FALSE;
            end if;
        elsif op[self] = "none" /\ ~starved[self] then
            if free > 0 then
                \* A receive into a buffer of the pool, which it holds while it waits.
                free := free - 1;
                op[self] := "recv";
            else
                \* The pool has none: the reader is starved, and asks for a poll.
                starved[self] := TRUE;
                starvedOnce := TRUE;
                if Mutation # "park" then
                    op[self] := "poll";
                end if;
            end if;
            woken[self] := FALSE;
        else
            \* An operation is out, or the reader waits for a buffer: it waits to be woken.
            woken[self] := FALSE;
        end if;
R_wait:
        if ~got[self] then
            await woken[self];
        end if;
    end while;
\* The reply, once the request is read.
W_loop:
    while ~replied[self] /\ ~sending[self] do
W_poll:
        \* MUTATION park_write: a starved writer waits for a buffer to come back, and nothing else.
        if wstarved[self] /\ ~wpoll[self] /\ Mutation # "park_write" then
            \* The poll said the socket takes bytes: the reply goes from the task's own buffer.
            replied[self] := TRUE;
            direct := TRUE;
        elsif ~wstarved[self] then
            if free > 0 then
                \* The reply is copied into a buffer of the pool, which the ring sends out.
                free := free - 1;
                sending[self] := TRUE;
            else
                \* The pool has none: the writer is starved, and asks for a poll for room.
                wstarved[self] := TRUE;
                starvedOnce := TRUE;
                if Mutation # "park_write" then
                    wpoll[self] := TRUE;
                end if;
                woken[self] := FALSE;
            end if;
        else
            \* Its poll is out, or it waits for a buffer: it waits to be woken.
            woken[self] := FALSE;
        end if;
W_wait:
        if ~replied[self] /\ ~sending[self] then
            await woken[self];
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

\* Every buffer of the pool is free, held by a receive out, holds a request read ahead, or a
\* reply not sent yet.
Buffers == free + Cardinality({c \in Conns : op[c] = "recv"})
               + Cardinality({c \in Conns : ahead[c]})
               + Cardinality({c \in Conns : sending[c]}) = Pool

\* Every request a peer sent is read and answered.
Served == <>(\A c \in Active : got[c] /\ replied[c])

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A reader found the pool empty.
NoStarve == ~starvedOnce
\* A starved reader read its request into its own buffer, or a starved writer wrote its reply
\* from its own.
NoDirect == ~direct
=============================================================================

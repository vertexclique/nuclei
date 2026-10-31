----------------------------- MODULE PortStarve -----------------------------
(***************************************************************************)
(* Stream reads and replies on a completion port that share one shard's    *)
(* pool of IO buffers (src/io/source/read/ops.rs `submit_ops`,             *)
(* `submit_wanted` and `op_done`; the zero-byte receive in                 *)
(* src/io/source/port.rs `stream_memory` and `outcome`; `buffer`,          *)
(* `unstarve` and the starved reader's own call `read_starved` in          *)
(* src/io/source/read.rs; `write`, `write_room` and `buffer` in            *)
(* src/io/source/write.rs, and the room send's end in                      *)
(* src/io/source/write/ops.rs `op_done`), at the grain of a step under     *)
(* each side's guard.                                                      *)
(*                                                                         *)
(* In plain words: several connections each have a task that reads one     *)
(* request, writes one reply, and then asks for the next request, which    *)
(* never comes. Some peers never send at all; some tasks take a peek at    *)
(* their request and then never read it, so its buffer stays read ahead. A *)
(* receive into a buffer of the pool waits in the kernel until its peer    *)
(* sends, holding that buffer all the while, so idle peers and stopped     *)
(* tasks may hold every buffer. A read the pool has no buffer for sends a  *)
(* zero-byte receive, which holds nothing; once that ends (the request is  *)
(* in the socket) a receive takes a buffer, which ends at once since the   *)
(* bytes are there, sent out at once on the thread that took the end. If   *)
(* the pool still has no buffer, the reader is starved, is woken, and      *)
(* reads the request straight from the socket into its own buffer: the     *)
(* zero-byte receive said it is there, so that call never waits. A reply   *)
(* is copied into a buffer of the pool that the kernel sends out and gives *)
(* back; a writer the pool has none for copies it into its slot's own room *)
(* and sends it from there. A buffer that comes back wakes every starved   *)
(* side to try the pool again.                                             *)
(*                                                                         *)
(* What must never happen: a reader's own call on a socket that has        *)
(* nothing for it, which would block its thread; or a peer's request that  *)
(* is never read, or never answered, because every buffer of the pool      *)
(* waits in the kernel for peers that never send or holds bytes no task    *)
(* reads (what tests/idle_peers.rs found on Windows: receives of idle      *)
(* peers held every buffer of a 1 MiB cap, and no other peer got a reply). *)
(*                                                                         *)
(* What is abstract: the guards (each step runs whole under its side's),   *)
(* the port's thread (each end is applied as the kernel makes it), the     *)
(* sizes (every buffer one class, a reply fits the room), and the bytes:   *)
(* one request and one reply per connection, each whole.                   *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Idle,      \* how many of the connections have peers that never send
    Held,      \* how many of the next connections have tasks that peek and stop
    Mutation   \* "none", or the name of the one rule this run takes away

\* The connections, and the buffers the pool holds: fewer than the connections.
Conns == 1..3
Pool == 2
\* The connections whose peers send their request; the first `Idle` never do.
Active == {c \in Conns : c > Idle}
\* Of those, the ones whose tasks peek at the request and then never read it.
Holders == {c \in Conns : Idle < c /\ c <= Idle + Held}

(* --algorithm PortStarve
variables
    free = Pool,                        \* buffers of the pool no one holds
    sock = [c \in Conns |-> 0],         \* a request in the socket, not received yet
    op = [c \in Conns |-> "none"],      \* the read's operation: "none", "zero" (holds nothing), "recv" (holds a buffer)
    readable = [c \in Conns |-> FALSE], \* a zero-byte receive said the request is in the socket
    ahead = [c \in Conns |-> FALSE],    \* a request received into a buffer of the pool
    starved = [c \in Conns |-> FALSE],  \* the pool had no buffer for the read
    woken = [c \in Conns |-> FALSE],    \* the waiting task was woken
    got = [c \in Conns |-> FALSE],      \* the task has its request
    sending = [c \in Conns |-> FALSE],  \* a reply copied into a buffer of the pool, not sent yet
    wstarved = [c \in Conns |-> FALSE], \* the pool had no buffer for the reply
    replied = [c \in Conns |-> FALSE],  \* the reply went out
    room = [c \in Conns |-> FALSE],     \* a reply sent from the slot's own room, not sent yet
    starvedOnce = FALSE,                \* (record) a side found the pool empty
    direct = FALSE,                     \* (record) a starved side used its own memory
    blockedOnce = FALSE;                \* (record) a reader's own call met a socket with nothing

\* A read with nothing out asks the kernel for its request (`submit_ops`).
macro read(c) begin
    if readable[c] then
        \* A zero-byte receive said the request is there.
        if free > 0 then
            \* A receive into a buffer of the pool: it ends at once.
            free := free - 1;
            \* The receive is out.
            op[c] := "recv";
            \* MUTATION keep_readable: going out, the receive leaves the side marked readable.
            if Mutation # "keep_readable" then
                \* The bytes the zero-byte receive saw are this receive's.
                readable[c] := FALSE;
            end if;
        else
            \* The pool has none: the read is starved, and its task reads the request itself.
            starved[c] := TRUE;
            \* (record) a side found the pool empty.
            starvedOnce := TRUE;
        end if;
    elsif free > 0 then
        \* A receive into a buffer of the pool, which it holds while it waits for the request.
        free := free - 1;
        \* The receive is out.
        op[c] := "recv";
    else
        \* The pool is empty: a zero-byte receive, which holds no buffer.
        op[c] := "zero";
    end if;
end macro;

\* A read the pool has no buffer for, with nothing out, calls on its socket itself
\* (`read_starved`), once a zero-byte receive said the request is there.
macro own(c) begin
    \* MUTATION park: a starved reader waits for a buffer to come back, and nothing else.
    \* MUTATION own_early: a starved reader calls whatever it has out and the socket was seen to hold.
    if starved[c] /\ free = 0 /\ Mutation # "park"
       /\ ((op[c] = "none" /\ readable[c]) \/ Mutation = "own_early") then
        if sock[c] = 1 then
            \* Straight from the socket into the task's own buffer: the call ends at once.
            got[c] := TRUE;
            \* The socket is empty again.
            sock[c] := 0;
            \* The bytes the zero-byte receive saw are taken.
            readable[c] := FALSE;
            \* (record) a starved side used its own memory.
            direct := TRUE;
        else
            \* (record) the call met an empty socket: on a blocking socket it would wait.
            blockedOnce := TRUE;
        end if;
    end if;
end macro;

\* The peers that send: each request reaches its socket, once.
fair process peer = 0
variables left = Active;    \* the peers that have not sent yet
begin
P_send:
    while left # {} do
        with c \in left do
            \* The request is in the socket now.
            sock[c] := 1;
            \* That peer has sent.
            left := left \ {c};
        end with;
    end while;
end process;

\* The kernel ends the operations out, and the port's thread applies each end at once.
fair process kernel = 4
begin
K_loop:
    while TRUE do
        either
            \* A reply in a buffer of the pool goes out.
            with c \in {d \in Conns : sending[d]} do
                \* The reply is no longer waiting to go.
                sending[c] := FALSE;
                \* The peer has its reply.
                replied[c] := TRUE;
                \* Its buffer comes back to the pool.
                free := free + 1;
                \* MUTATION no_unstarve: a buffer that comes back wakes nobody.
                if Mutation # "no_unstarve" then
                    \* Every starved side is woken to try the pool again.
                    woken := [d \in Conns |-> woken[d] \/ starved[d] \/ wstarved[d]];
                    \* No side is starved any more: each will ask the pool again.
                    starved := [d \in Conns |-> FALSE];
                    \* The same for the writers.
                    wstarved := [d \in Conns |-> FALSE];
                end if;
            end with;
        or
            \* A reply sent from the slot's own room goes out: it held no buffer of the pool.
            with c \in {d \in Conns : room[d]} do
                \* Nothing waits to go any more.
                room[c] := FALSE;
                \* The peer has its reply.
                replied[c] := TRUE;
            end with;
        or
            \* A receive into a buffer of the pool takes the request that is in its socket.
            with c \in {d \in Conns : op[d] = "recv" /\ sock[d] = 1} do
                \* The request is in the buffer now, for the task to take.
                ahead[c] := TRUE;
                \* The socket is empty again.
                sock[c] := 0;
                \* The receive is over.
                op[c] := "none";
                \* The task is woken to take the request.
                woken[c] := TRUE;
            end with;
        or
            \* A zero-byte receive ends: the request is in the socket, and stays there.
            with c \in {d \in Conns : op[d] = "zero" /\ sock[d] = 1} do
                \* MUTATION no_resubmit: its end sends nothing out and wakes nobody.
                if Mutation = "no_resubmit" then
                    \* The side knows the request came...
                    readable[c] := TRUE;
                    \* ...and its zero-byte receive is over, but nothing more happens.
                    op[c] := "none";
                elsif free > 0 then
                    \* The receive into a buffer of the pool goes out at once (`submit_wanted`).
                    free := free - 1;
                    \* The receive is out, and ends at once since the bytes are there.
                    op[c] := "recv";
                    \* Going out, it clears the mark (unless keep_readable takes that away).
                    readable[c] := (Mutation = "keep_readable");
                else
                    \* The pool has none: the side remembers the request came...
                    readable[c] := TRUE;
                    \* ...has nothing out...
                    op[c] := "none";
                    \* ...and is starved.
                    starved[c] := TRUE;
                    \* (record) a side found the pool empty.
                    starvedOnce := TRUE;
                    \* MUTATION no_starved_wake: the task is not woken to read the bytes itself.
                    if Mutation # "no_starved_wake" then
                        \* Its task is woken to read the request into its own buffer.
                        woken[c] := TRUE;
                    end if;
                end if;
            end with;
        end either;
    end while;
end process;

\* Each connection's task: its read, polled again each time it is woken, until it has its
\* request; then its reply; then the read of a next request its peer never sends.
fair process reader \in Conns
begin
R_loop:
    while ~got[self] do
R_poll:
        if ahead[self] /\ self \in Holders then
            \* A task that peeked: it never takes the request, which stays read ahead.
            woken[self] := FALSE;
        elsif ahead[self] then
            \* The request came into a buffer of the pool: the task takes it.
            got[self] := TRUE;
            \* Nothing is read ahead any more.
            ahead[self] := FALSE;
            \* The buffer goes back to the pool.
            free := free + 1;
            if Mutation # "no_unstarve" then
                \* Every starved side is woken to try the pool again.
                woken := [c \in Conns |-> woken[c] \/ starved[c] \/ wstarved[c]];
                \* No side is starved any more.
                starved := [c \in Conns |-> FALSE];
                \* The same for the writers.
                wstarved := [c \in Conns |-> FALSE];
            end if;
        elsif op[self] = "none" /\ ~starved[self] then
            \* Nothing is out: the read asks the kernel for its request.
            read(self);
            \* The task waits to be woken.
            woken[self] := FALSE;
        else
            \* An operation is out, or the read waits for a buffer: the task waits to be woken.
            woken[self] := FALSE;
        end if;
R_own:
        if ~got[self] then
            own(self);
        end if;
R_wait:
        if ~got[self] then
            \* Asleep until something wakes it.
            await woken[self];
        end if;
    end while;
\* A task that peeked never replies: it holds its request for good.
R_hold:
    await self \notin Holders;
\* The reply, once the request is read.
W_loop:
    while ~replied[self] /\ ~sending[self] /\ ~room[self] do
W_poll:
        if ~wstarved[self] then
            if free > 0 then
                \* The reply is copied into a buffer of the pool, which the kernel sends out.
                free := free - 1;
                \* The reply waits in the kernel to go out.
                sending[self] := TRUE;
            else
                \* The pool has none: the writer is starved.
                wstarved[self] := TRUE;
                \* (record) a side found the pool empty.
                starvedOnce := TRUE;
                \* MUTATION park_write: a starved writer waits for a buffer to come back.
                if Mutation # "park_write" then
                    \* The reply goes into the slot's own room, sent from there.
                    room[self] := TRUE;
                    \* (record) a starved side used its own memory.
                    direct := TRUE;
                end if;
                \* The task waits to be woken.
                woken[self] := FALSE;
            end if;
        else
            \* It waits for a buffer to come back: the task waits to be woken.
            woken[self] := FALSE;
        end if;
W_wait:
        if ~replied[self] /\ ~sending[self] /\ ~room[self] then
            \* Asleep until something wakes it.
            await woken[self];
        end if;
    end while;
\* The read of the next request, which the peer never sends.
R_next:
    read(self);
R_next_own:
    own(self);
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

\* A reader's own call on its socket never meets one with nothing, where it would block.
NeverBlocks == ~blockedOnce

\* Every request a peer sent is read and answered, but those of tasks that peeked and stopped.
Served == <>(\A c \in Active \ Holders : got[c] /\ replied[c])

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A side found the pool empty.
NoStarve == ~starvedOnce
\* A read waits in a zero-byte receive.
NoZero == \A c \in Conns : op[c] # "zero"
\* A starved reader read straight from its socket, or a starved writer sent from its room.
NoDirect == ~direct
=============================================================================

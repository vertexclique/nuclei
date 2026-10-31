------------------------------ MODULE FdStarve ------------------------------
(***************************************************************************)
(* Stream reads on a readiness poller (epoll, kqueue) that share one       *)
(* shard's pool of IO buffers (src/io/source/read.rs: the driver's `serve` *)
(* and `leaves_to_tasks`, `complete_fd` and `read_ahead`; the task's       *)
(* `read`, `reads_itself`, `read_direct` and `look_again`; `buffer` and    *)
(* `unstarve`), at the grain of a step under the side's guard.             *)
(*                                                                         *)
(* In plain words: several connections each have a task that peeks at one *)
(* request and then reads it. A read too small to go straight into the     *)
(* task's buffer has the driver read ahead into a buffer of the pool, and  *)
(* the task copies the bytes out. A peek leaves the bytes where they are,  *)
(* so a task that peeks and then stops reading (it waits for something     *)
(* else) keeps its buffer for as long as it waits. When the pool has no    *)
(* buffer left, a reader is starved: it reads, or peeks, straight from the *)
(* socket into its own buffer. The poller says nothing new until new bytes *)
(* come (an edge): a starved reader that finds the socket empty waits for  *)
(* the edge, and the edge wakes it to read itself; one that just found the *)
(* pool empty while the socket may hold bytes looks again at once. A       *)
(* buffer that comes back wakes every starved reader to try the pool       *)
(* again.                                                                  *)
(*                                                                         *)
(* What must never happen: a request that is never read because every      *)
(* buffer of the pool holds the bytes of tasks that stopped reading. Before *)
(* the fix a starved reader on epoll waited for a buffer to come back, and *)
(* none ever did.                                                          *)
(*                                                                         *)
(* What is abstract: the guards (each step runs whole under its side's),   *)
(* the driver's thread (each edge is served as one step), and the bytes:   *)
(* one request per connection, read whole.                                 *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Holders,   \* how many of the connections have tasks that peek and then never read
    Mutation   \* "none", or the name of the one rule this run takes away

\* The connections, and the buffers the pool holds: fewer than the connections.
Conns == 1..2
Pool == 1
\* The connections whose tasks read their request; the first `Holders` only peek at theirs.
Active == {c \in Conns : c > Holders}
\* Where a task goes after its peek: an active one on to its read, a holder nowhere.
AfterPeek(c) == IF c \in Active THEN "read" ELSE "hold"

(* --algorithm FdStarve
variables
    free = Pool,                        \* buffers of the pool no one holds
    sock = [c \in Conns |-> 0],         \* a request in the socket, not read into anything yet
    edge = [c \in Conns |-> FALSE],     \* the poller has an edge for the socket the driver has not served
    blocked = [c \in Conns |-> FALSE],  \* the last call found the socket empty, and no edge came since
    ahead = [c \in Conns |-> FALSE],    \* a request read ahead into a buffer of the pool
    starved = [c \in Conns |-> FALSE],  \* the pool had no buffer for the side
    waits = [c \in Conns |-> FALSE],    \* the side's task waits to be woken
    woken = [c \in Conns |-> FALSE],    \* the waiting task was woken
    phase = [c \in Conns |-> "peek"],   \* the task's next call: "peek", "read"; then "done" or "hold"
    direct = FALSE,                     \* (record) a starved reader called on the socket itself
    starvedOnce = FALSE;                \* (record) a reader found the pool empty

\* Each peer sends its one request, which brings an edge.
fair process peer = 0
variables left = Conns;    \* the peers that have not sent yet
begin
P_send:
    while left # {} do
        with c \in left do
            \* The request is in the socket, and the poller reports it.
            sock[c] := 1;
            edge[c] := TRUE;
            left := left \ {c};
        end with;
    end while;
end process;

\* The shard's driver serves each edge (`serve`).
fair process driver = 4
begin
D_loop:
    while TRUE do
        with c \in {d \in Conns : edge[d]} do
            \* The edge is taken: the socket may hold bytes again.
            edge[c] := FALSE;
            if waits[c] /\ ~woken[c] then
                if starved[c] then
                    \* A starved side's bytes are its tasks' to read (`leaves_to_tasks`): they are woken.
                    blocked[c] := FALSE;
                    woken[c] := TRUE;
                elsif free > 0 then
                    if sock[c] = 1 then
                        \* The driver reads the request ahead into a buffer of the pool, and wakes the task.
                        free := free - 1;
                        ahead[c] := TRUE;
                        sock[c] := 0;
                        blocked[c] := TRUE;
                        woken[c] := TRUE;
                    else
                        \* Nothing came after all: the buffer goes back, and the task waits on.
                        blocked[c] := TRUE;
                    end if;
                else
                    \* The pool has none: the side is starved, and its task is woken to read itself.
                    \* MUTATION edge_quiet: the edge that starves the side wakes nobody.
                    blocked[c] := FALSE;
                    starved[c] := TRUE;
                    starvedOnce := TRUE;
                    woken[c] := (Mutation # "edge_quiet");
                end if;
            else
                \* No task waits: the bytes stay for the next read.
                blocked[c] := FALSE;
            end if;
        end with;
    end while;
end process;

\* Each connection's task: its peek and its read, each polled again whenever it is woken.
fair process reader \in Conns
begin
R_loop:
    while phase[self] \in {"peek", "read"} do
R_poll:
        \* MUTATION park: a starved reader waits for a buffer to come back, and calls nothing itself.
        if starved[self] /\ Mutation # "park" /\ ~ahead[self] /\ ~blocked[self] then
            \* The starved reader calls on the socket itself, into its own buffer (`read_direct`).
            if sock[self] = 1 then
                direct := TRUE;
                if phase[self] = "peek" then
                    \* The peek copies the request and leaves it in the socket.
                    phase[self] := AfterPeek(self);
                    \* MUTATION peek_drains: the peek is taken as having emptied the socket.
                    if Mutation = "peek_drains" then
                        blocked[self] := TRUE;
                    end if;
                else
                    \* The read takes the request, and empties the socket.
                    phase[self] := "done";
                    sock[self] := 0;
                    blocked[self] := TRUE;
                end if;
            else
                \* The socket is empty: the task waits for the next edge.
                blocked[self] := TRUE;
                waits[self] := TRUE;
                woken[self] := FALSE;
            end if;
        elsif ahead[self] then
            if phase[self] = "peek" then
                \* The peek copies the request read ahead, and leaves it there.
                phase[self] := AfterPeek(self);
            else
                \* The read takes the request: its buffer goes back, which wakes every starved
                \* task that waits to try the pool again (`unstarve`).
                phase[self] := "done";
                ahead[self] := FALSE;
                free := free + 1;
                woken := [d \in Conns |-> woken[d] \/ (starved[d] /\ waits[d])];
                starved := [d \in Conns |-> FALSE];
            end if;
        elsif ~blocked[self] then
            if free > 0 then
                if sock[self] = 1 then
                    \* The driver reads the request ahead for the task, into a buffer of the pool;
                    \* the task takes it on its next step.
                    free := free - 1;
                    ahead[self] := TRUE;
                    sock[self] := 0;
                    blocked[self] := TRUE;
                else
                    \* The socket is empty: the buffer goes back, and the task waits for the next edge.
                    blocked[self] := TRUE;
                    waits[self] := TRUE;
                    woken[self] := FALSE;
                end if;
            else
                \* The pool has none: the reader is starved. The socket may hold bytes no edge will
                \* report again, so it looks again at once, reading itself (`look_again`).
                \* MUTATION no_look: it waits for the next edge instead.
                starved[self] := TRUE;
                starvedOnce := TRUE;
                waits[self] := TRUE;
                woken[self] := (Mutation # "no_look");
            end if;
        else
            \* The socket was found empty and no edge came since: the task waits for the next one.
            waits[self] := TRUE;
            woken[self] := FALSE;
        end if;
R_wait:
        \* A task that waits goes on once it is woken.
        if waits[self] then
            await woken[self];
            waits[self] := FALSE;
            woken[self] := FALSE;
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

\* Every buffer of the pool is free or holds a request read ahead.
Buffers == free + Cardinality({c \in Conns : ahead[c]}) = Pool

\* Every active task reads its request.
Read == <>(\A c \in Active : phase[c] = "done")

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A reader found the pool empty.
NoStarve == ~starvedOnce
\* A starved reader peeked or read straight from the socket.
NoDirect == ~direct
=============================================================================

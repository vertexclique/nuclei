----------------------------- MODULE DgramQuick -----------------------------
(***************************************************************************)
(* A datagram socket's receive made at once, with no guard, while its read *)
(* side holds nothing, on epoll or kqueue: the call with no guard in       *)
(* src/io/source/read/calls.rs `poll_receive_as`; the word of             *)
(* src/io/source/quick.rs, which `quick_word` (src/io/source/read/         *)
(* datagram.rs) computes and `releasing` (src/io/source/hooks.rs) leaves   *)
(* as each holder lets go; the call under the guard in `receive_now`,      *)
(* reached through `poll_receive_held`; the edge in `serve`                *)
(* (src/io/source/read.rs); an owned receive's lent buffer that            *)
(* `receive_lent` fills and `abandon_lent` hands to the side.              *)
(*                                                                          *)
(* In plain words: a peer sends datagrams, and the kernel keeps them in    *)
(* order. Each arrival makes an edge, which the driver serves under the    *)
(* side's guard: it counts the edge, fills a lent buffer that waits, and   *)
(* wakes a task that waits. A task first starts an owned receive, whose    *)
(* buffer the side keeps for the driver to fill, and drops it, maybe after *)
(* the driver put a datagram in it: the side then holds that datagram for  *)
(* the next receive. Then the task receives the rest, one call after       *)
(* another. Each call first reads the word the side's last holder left.    *)
(* When it says the side holds nothing, the call asks the kernel at once,  *)
(* with no guard. A call the kernel answers "not now", or one the word     *)
(* sends to the side, takes the guard: what the side holds comes first; a  *)
(* call that asked already waits without asking again when no edge was     *)
(* counted since the word it saw, and asks again otherwise; with nothing   *)
(* there it waits for the next edge.                                       *)
(*                                                                          *)
(* What must never happen: datagrams received out of the order the peer    *)
(* sent them, or a call that waits forever.                                *)
(*                                                                          *)
(* What is abstract: a datagram's bytes (each is its number), and the      *)
(* guard's own protocol (side/ and side_biased/ check it): a holder holds  *)
(* the side whole, and an edge that finds it held is left for the holder,  *)
(* which serves it before it lets go and then runs its step again if its   *)
(* call still waits.                                                       *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    Mutation   \* "none", or the name of the one rule this run takes away

\* How many datagrams the peer sends.
N == 3
\* What the owned receive's buffer holds: nothing lent, or lent and waiting for a datagram.
NoBuffer == 0
Waiting == -1
\* The word a call saw, when it saw none: the call went to the side without asking the kernel.
NoWord == [edges |-> -1, holds |-> TRUE]

(* --algorithm DgramQuick
variables
    sent = 0,              \* datagrams the peer sent so far
    socket = <<>>,         \* datagrams the kernel holds, oldest first, by number
    event = FALSE,         \* an edge epoll has to report: one arrival, or many since the last
    guard = "none",        \* who holds the side: "none" or "task" (the driver's turn is one step)
    ready = FALSE,         \* an edge came while the task held the side, left for it to serve
    blocked = FALSE,       \* the last call found the kernel empty, and no edge came since
    edges = 0,             \* the edges the side counted
    held = <<>>,           \* datagrams the side holds for the next receive, oldest first
    lent = NoBuffer,       \* the owned receive's buffer: NoBuffer, Waiting, or the datagram in it
    waiter = FALSE,        \* the task waits for a wake
    woken = FALSE,         \* the task was woken since it last went to the side
    word = [edges |-> 0, holds |-> FALSE],  \* what the side's last holder left
    got = <<>>,            \* what the task received, in order
    quickGot = FALSE,      \* (record) a call with no guard received a datagram
    seenWait = FALSE,      \* (record) a call waited on the word it saw, with no second ask
    heldGot = FALSE;       \* (record) a receive took a datagram the side held

define
    \* Whether a call must go through the side, which it must while the side holds a datagram
    \* or a lent buffer, or the last call found the kernel empty and no edge came since.
    \* MUTATION no_holds: the word forgets what the side holds.
    Holds(h, l, b) == IF Mutation = "no_holds" THEN b ELSE h # <<>> \/ l # NoBuffer \/ b
    \* What the side's holder leaves as it lets go: its count of edges, and whether a call must
    \* go through the side.
    Word(e, h, l, b) == [edges |-> e, holds |-> Holds(h, l, b)]
    \* Whether no edge was counted since the word `w` a call saw.
    \* MUTATION no_edge_check: a call that asked already believes no edge came.
    \* MUTATION published_edges: it compares with the word left, which a hold has not updated.
    Unchanged(w) == CASE Mutation = "no_edge_check" -> TRUE
                      [] Mutation = "published_edges" -> w.edges = word.edges
                      [] OTHER -> w.edges = edges
end define;

\* The driver's part for an edge, under the guard: the edge is counted, a lent buffer that waits
\* gets the oldest datagram, and a waiting task is woken.
macro Serve() begin
    blocked := FALSE;
    edges := edges + 1;
    if lent = Waiting /\ socket # <<>> then
        lent := Head(socket);
        socket := Tail(socket);
    end if;
    if waiter then
        waiter := FALSE;
        woken := TRUE;
    end if;
end macro;

\* The peer sends its datagrams, one after another; each arrival makes an edge.
fair process peer = "peer"
begin
P_send:
    while sent < N do
        \* One more datagram reaches the kernel, and epoll has an edge to report.
        sent := sent + 1;
        socket := Append(socket, sent);
        event := TRUE;
    end while;
end process;

\* The driver: it serves each edge epoll reports, at once if the side is free, else it leaves it
\* to the holder.
fair process driver = "driver"
begin
D_loop:
    while TRUE do
        \* Epoll reports the edge, once for all the arrivals since the last report.
        await event;
        event := FALSE;
        if guard = "none" then
            \* The side is free: the driver takes it, serves the edge and leaves its word.
            Serve();
            word := Word(edges, held, lent, blocked);
        else
            \* The task holds it: the edge is left for the task to serve before it lets go.
            ready := TRUE;
        end if;
    end while;
end process;

\* The task: an owned receive it drops, then a receive for each datagram left.
fair process task = "task"
variables
    seen = NoWord,   \* the word a call with no guard saw before the kernel said "not now"
    done = FALSE;    \* the call under way received its datagram
begin
T_own:
    \* The owned receive's first poll, under the guard: a datagram there is its own at once;
    \* else the side keeps its buffer for the driver to fill, and the kernel was empty.
    await guard = "none";
    if ~blocked /\ socket # <<>> then
        \* The kernel had one: the owned receive takes it and ends.
        got := Append(got, Head(socket));
        socket := Tail(socket);
    else
        \* None there: the buffer waits with the side, and the kernel was found empty.
        blocked := TRUE;
        lent := Waiting;
    end if;
    \* The task lets go, leaving its word.
    word := Word(edges, held, lent, blocked);
T_drop:
    \* The task drops the owned receive, at once or after the driver put a datagram in its
    \* buffer: the side holds that datagram for the next receive.
    await guard = "none";
    if lent > 0 then
        \* The datagram in the dropped buffer is the side's, for the next receive.
        held := Append(held, lent);
    end if;
    \* The buffer is gone, and the task lets go, leaving its word.
    lent := NoBuffer;
    word := Word(edges, held, lent, blocked);
T_loop:
    while Len(got) < N do
T_look:
        \* The call reads the word the side's last holder left.
        if word.holds then
            \* The side holds something, or waits for an edge: the call goes to it.
            seen := NoWord;
            goto T_take;
        else
            seen := word;
        end if;
T_ask:
        \* The side holds nothing: the call asks the kernel at once, with no guard.
        if socket # <<>> then
            \* The kernel has one: it is the call's.
            got := Append(got, Head(socket));
            socket := Tail(socket);
            quickGot := TRUE;
            goto T_loop;
        end if;
T_take:
        \* The kernel said "not now", or the word sent the call here: it takes the guard.
        await guard = "none";
        guard := "task";
        \* A new wait starts, and the call has nothing yet.
        woken := FALSE;
        done := FALSE;
T_step:
        \* The call's step under the guard (run again after an edge served before letting go).
        if held # <<>> then
            \* What the side holds comes first.
            got := Append(got, Head(held));
            held := Tail(held);
            done := TRUE;
            heldGot := TRUE;
        elsif ~blocked /\ ~(~seen.holds /\ Unchanged(seen)) /\ socket # <<>> then
            \* The call asks the kernel again, which has a datagram for it.
            got := Append(got, Head(socket));
            socket := Tail(socket);
            done := TRUE;
        else
            \* Nothing for the call: no edge since a "not now", or the kernel is empty. It waits
            \* for the next edge.
            if ~blocked /\ ~seen.holds /\ Unchanged(seen) then
                \* (record) it waits on the word it saw, with no second ask.
                seenWait := TRUE;
            end if;
            \* It waits for the next edge, which wakes it.
            blocked := TRUE;
            waiter := TRUE;
        end if;
T_release:
        \* Letting go: an edge left meanwhile is served first, and a call still waiting runs its
        \* step again; else the task leaves its word and lets go, in one step.
        if ready then
            \* The edge left meanwhile, served by the task.
            ready := FALSE;
            Serve();
            if done then
                goto T_release;
            else
                goto T_step;
            end if;
        else
            \* No edge left: the word goes out and the side is free.
            word := Word(edges, held, lent, blocked);
            guard := "none";
        end if;
T_wait:
        \* A call that waits does so until a wake, then starts over.
        if ~done then
            await woken;
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

\* The datagrams are received in the order the peer sent them, none twice and none skipped.
InOrder == \A j \in 1..Len(got) : got[j] = j

\* Every call ends: the task receives every datagram.
Termination == <>(pc["task"] = "Done")

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A call with no guard received a datagram.
NoQuickGot == ~quickGot
\* A call waited on the word it saw, without asking the kernel again.
NoSeenWait == ~seenWait
\* A receive took a datagram the side held from the dropped owned receive.
NoHeldGot == ~heldGot
=============================================================================

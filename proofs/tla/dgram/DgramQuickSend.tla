--------------------------- MODULE DgramQuickSend ---------------------------
(***************************************************************************)
(* A datagram socket's send made at once, with no guard, while its write   *)
(* side holds nothing, on epoll or kqueue: the call with no guard in       *)
(* src/io/source/write/calls.rs `poll_send`; the word of                  *)
(* src/io/source/quick.rs, which `quick_holds` (src/io/source/write.rs)    *)
(* computes and `releasing` (src/io/source/hooks.rs) leaves as each holder *)
(* lets go, and `hold_write` raises before the side's own call; the call   *)
(* under the guard in `send_now` (src/io/source/write/datagram.rs); the    *)
(* driver's writable edge in `on_event` (src/io/source.rs) and `serve`.    *)
(*                                                                          *)
(* In plain words: a task sends datagrams, one call after another, into a  *)
(* socket that holds one; one from before fills it at the start. A peer    *)
(* reads what the socket holds, and each read makes room, which epoll      *)
(* reports as a writable edge. Each call first reads the word the side's   *)
(* last holder left. When it says the side holds and waits for nothing,    *)
(* the call hands its datagram to the kernel at once, with no guard. A     *)
(* call the kernel answers "not now", or one the word sends to the side,   *)
(* takes the guard, raises the word, and asks the kernel (again); with no  *)
(* room it waits for the next writable edge. The driver lets a writable    *)
(* edge go by while the word is clear: nobody waits for room then.         *)
(*                                                                          *)
(* What must never happen: the task's datagrams reach the peer out of the  *)
(* order it sent them, or a call that waits forever.                       *)
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

\* How many datagrams the task sends.
N == 3
\* How many datagrams the socket holds before it answers "not now".
Room == 1

(* --algorithm DgramQuickSend
variables
    socket = <<0>>,        \* datagrams the kernel holds for the peer, oldest first; 0 is from before
    wire = <<>>,           \* datagrams the peer read, in order
    event = FALSE,         \* a writable edge epoll has to report
    guard = "none",        \* who holds the side: "none" or "task" (the driver's turn is one step)
    ready = FALSE,         \* an edge came while the task held the side, left for it to serve
    blocked = FALSE,       \* the last call found the socket full, and no edge came since
    waiter = FALSE,        \* the task waits for a wake
    woken = FALSE,         \* the task was woken since it last went to the side
    holds = FALSE,         \* the side's word: a send goes through the side, and an edge reaches it
    quickSent = FALSE,     \* (record) a call with no guard handed its datagram to the kernel
    skipped = FALSE,       \* (record) the driver let a writable edge go by
    waited = FALSE;        \* (record) a call waited for room

define
    \* The word a holder leaves as it lets go: raised while a call found no room and no edge
    \* came since, or a call waits.
    \* MUTATION forget_wait: the word left forgets a call that waits for room.
    Left(b, w) == IF Mutation = "forget_wait" THEN FALSE ELSE b \/ w
end define;

\* The driver's part for a writable edge, under the guard: a call that waits is woken.
macro Serve() begin
    blocked := FALSE;
    if waiter then
        waiter := FALSE;
        woken := TRUE;
    end if;
end macro;

\* The peer reads the oldest datagram the socket holds; the room it makes is a writable edge.
fair process peer = "peer"
begin
P_read:
    while TRUE do
        \* Something to read: it comes off the socket onto the wire, and makes room.
        await socket # <<>>;
        wire := Append(wire, Head(socket));
        socket := Tail(socket);
        event := TRUE;
    end while;
end process;

\* The driver: each writable edge epoll reports reaches the side while its word is raised, at
\* once if the side is free, else left to the holder; with the word clear it goes by.
fair process driver = "driver"
begin
D_loop:
    while TRUE do
        \* Epoll reports the edge, once for all the reads since the last report.
        await event;
        event := FALSE;
        if ~holds then
            \* Nobody waits for room: the edge goes by.
            skipped := TRUE;
        elsif guard = "none" then
            \* The side is free: the driver takes it, serves the edge and leaves its word.
            Serve();
            holds := Left(blocked, waiter);
        else
            \* The task holds it: the edge is left for the task to serve before it lets go.
            ready := TRUE;
        end if;
    end while;
end process;

\* The task: a send for each datagram, in order.
fair process task = "task"
variables
    at = 1,          \* the datagram the call under way sends
    asked = FALSE,   \* the call asked the kernel with no guard, which said "not now"
    done = FALSE;    \* the call under way handed its datagram to the kernel
begin
T_loop:
    while at <= N do
T_look:
        \* The call reads the word the side's last holder left.
        asked := FALSE;
        if holds then
            \* The side holds or waits for something: the call goes to it.
            goto T_take;
        end if;
T_ask:
        \* The side holds and waits for nothing: the call asks the kernel at once, with no guard.
        if Len(socket) < Room then
            \* The kernel takes it, behind what the socket holds.
            socket := Append(socket, at);
            at := at + 1;
            quickSent := TRUE;
            goto T_loop;
        else
            \* No room: the kernel says "not now", and the call goes to the side.
            asked := TRUE;
        end if;
T_take:
        \* The kernel said "not now", or the word sent the call here: it takes the guard.
        await guard = "none";
        guard := "task";
        \* A new wait starts, and the call has sent nothing yet.
        woken := FALSE;
        done := FALSE;
T_raise:
        \* Under the guard, before the side's own call, the word is raised: from here every
        \* writable edge reaches the side.
        \* MUTATION no_raise: the word is raised only as the side lets go.
        if Mutation # "no_raise" then
            holds := TRUE;
        end if;
T_step:
        \* The call's step under the guard (run again after an edge served before letting go).
        \* MUTATION no_retry: a call the kernel said "not now" to waits without asking again.
        if ~blocked /\ ~(asked /\ Mutation = "no_retry") /\ Len(socket) < Room then
            \* The side's own call: the kernel takes the datagram.
            socket := Append(socket, at);
            at := at + 1;
            done := TRUE;
        else
            \* No room (or, mutated, no second ask): the call waits for the next writable edge.
            blocked := TRUE;
            waiter := TRUE;
            waited := TRUE;
        end if;
T_release:
        \* Letting go: an edge left meanwhile is served first, and a call still waiting runs its
        \* step again; else the task leaves the word and lets go, in one step.
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
            holds := Left(blocked, waiter);
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

\* The peer reads the task's datagrams in the order it sent them, after the one from before.
InOrder == \A j \in 1..Len(wire) : wire[j] = j - 1

\* Every call ends: the task hands the kernel every datagram.
Termination == <>(pc["task"] = "Done")

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A call with no guard handed its datagram to the kernel.
NoQuickSent == ~quickSent
\* The driver let a writable edge go by.
NoSkip == ~skipped
\* A call waited for room.
NoWait == ~waited
=============================================================================

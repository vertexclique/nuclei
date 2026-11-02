----------------------------- MODULE TcpConnect -----------------------------
(***************************************************************************)
(* A TCP connect on each backend (src/io/source/write.rs:                  *)
(* `WriteState::connect`, and `push_fd` on a readiness poller; on a ring  *)
(* and a port, the dial `submit_ops` sends in src/io/source/write/ops.rs   *)
(* and its end in `op_done`; src/io/source/ring.rs maps the ring's         *)
(* answers, EALREADY, EISCONN and ECANCELED among them; src/io/source/     *)
(* port.rs makes the port's `ConnectEx`).                                  *)
(*                                                                          *)
(* Who acts. A caller polls its connect, and may drop the future between   *)
(* polls (`Drops`). The kernel's socket, once a connect is under way,      *)
(* decides the handshake once: connected, or refused (`Refused`). The       *)
(* shard's driver: on epoll and kqueue it serves the socket's write edge;  *)
(* on a ring and a port it sends the side's operation and takes its        *)
(* completion. On a ring the source may move to another shard while its    *)
(* operation is out (`Moves`), and the move cancels it.                    *)
(*                                                                          *)
(* What can happen. On epoll and kqueue the side calls connect(2) itself:  *)
(* in progress, it waits for the socket's write edge, which comes once the *)
(* kernel decided, and asks the socket its error (`take_error`). On a ring *)
(* the side sends a connect operation: on a fresh socket it waits in the   *)
(* kernel for the decision and ends with it. A move cancels it (ECANCELED)  *)
(* while the handshake goes on: the side keeps its dial and sends it       *)
(* again, which then finds the socket connecting (EALREADY: the side polls *)
(* for the socket to be writable and asks its error), connected (EISCONN:  *)
(* connected), or refused (the refusal). On a port the side sends          *)
(* `ConnectEx`, which ends with the decision; a move never cancels it (it  *)
(* completes on the port it was sent to, `PortKeep.tla`).                  *)
(*                                                                          *)
(* What must never happen. The caller hears another verdict than its       *)
(* socket's: an error for a connect that connects, or Ok for one refused   *)
(* (`OneResult`). What must happen: a caller that keeps polling hears its *)
(* verdict (`Termination`).                                                *)
(*                                                                          *)
(* What is abstract. The kernel's decision is one step that comes in the   *)
(* end (a handshake that is never answered times out in the kernel); a    *)
(* refused socket that is dialled again answers the refusal (the socket's *)
(* own error); the side's guard is a step's grain; a move is one cancel.  *)
(***************************************************************************)
EXTENDS Integers, TLC                                                        \* numbers and TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Backend,   \* "readiness" (epoll, kqueue), "ring" (io_uring) or "port" (IOCP)
    Refused,   \* TRUE: the peer may refuse the connection
    Moves,     \* TRUE: on a ring, the source moves once while its operation is out, which cancels it
    Drops,     \* TRUE: the caller may drop the future between polls
    Mutation   \* "none", or the name of the one rule this run takes away

(* --algorithm TcpConnect
variables
    sk = "idle",          \* the kernel's socket: "idle", "connecting", "connected" or "refused"
    started = FALSE,      \* the side's note that the caller's connect started (`connect_started`)
    connecting = FALSE,   \* the side waits for the connect's end (`connecting`)
    result = "none",      \* the connect's end, kept for the caller's next poll: "none", "ok", "refused", "error"
    dial = FALSE,         \* ring, port: a dial waits to be sent (`dial`)
    pollFirst = FALSE,    \* ring: the next operation is a poll for the socket to be writable (`poll_first`)
    op = "none",          \* ring, port: the side's operation in the kernel: "none", "connect" or "poll"
    found = "none",       \* what the connect operation found as it was sent: "wait", "already", "isconn", "refused"
    cancelled = FALSE,    \* ring: the move cancelled the operation out
    moved = FALSE,        \* ring: the source moved
    waiting = FALSE,      \* the caller's poll waits
    woken = FALSE,        \* the waiting poll was woken
    answered = "none",    \* what the caller heard: "none", "ok", "refused", "error" or "dropped"
    already = FALSE;      \* (record) a dial sent again found the socket still connecting

define                                                                       \* names the properties use:
    \* The kernel's verdict as the caller would hear it, once it decided.
    Verdict == IF sk = "connected" THEN "ok" ELSE IF sk = "refused" THEN "refused" ELSE "none"
end define;                                                                  \* end of the names

\* The connect's end reaches the side: kept for the caller, whose waiting poll is woken.
macro ended(r) begin
    connecting := FALSE;                                                     \* nothing more to wait for
    result := r;                                                             \* the end, for the next poll
    \* MUTATION no_wake: the end wakes nobody.
    if waiting /\ Mutation # "no_wake" then                                  \* a poll waits:
        woken := TRUE;                                                       \* it is woken
    end if;
end macro;

\* The caller's connect (`Handle<TcpStream>::connect` and its poll of the side).
fair process caller = "caller"
begin
C_loop:
    while answered = "none" do
        either                                                               \* the caller drops the future:
            await Drops;                                                     \* in the runs that allow it
            answered := "dropped";
        or                                                                   \* or polls the connect:
            if result # "none" then                                          \* its end is kept:
                answered := result;                                          \* the caller hears it
                started := started /\ result = "ok";                         \* a failed connect is forgotten
            elsif ~started then                                              \* the first poll starts it:
                started := TRUE;
                if Backend = "readiness" then                                \* connect(2), here:
                    either
                        sk := "connecting";                                  \* in progress:
                        connecting := TRUE;                                  \* the side waits for its end
                    or
                        sk := "connected";                                   \* or connected at once (a near peer):
                        answered := "ok";                                    \* the poll says so
                    end either;
                else                                                         \* a ring or a port:
                    connecting := TRUE;                                      \* the side waits for its end
                    dial := TRUE;                                            \* and the dial goes to the driver
                end if;
            elsif connecting then                                            \* under way:
                waiting := TRUE;                                             \* the poll waits
C_wait:
                await woken;                                                 \* until the end wakes it
                woken := FALSE;
                waiting := FALSE;
            else                                                             \* started, and no end to wait for
                answered := "ok";
            end if;
        end either;
    end while;
end process;

\* The kernel decides the handshake once a connect is under way.
fair process kernel = "kernel"
begin
K_decide:
    await sk = "connecting";                                                 \* a handshake under way
    either
        sk := "connected";                                                   \* the peer accepts
    or
        await Refused;                                                       \* or, where it may,
        sk := "refused";                                                     \* refuses
    end either;
end process;

\* The shard's driver.
fair process driver = "driver"
begin
D_loop:
    while TRUE do
        either                                                               \* epoll, kqueue: the write edge
            await Backend = "readiness" /\ connecting /\ Verdict # "none";   \* comes once the kernel decided
            ended(Verdict);                                                  \* `take_error` says how
        or                                                                   \* a ring or a port sends the dial:
            await Backend # "readiness" /\ dial /\ op = "none" /\ ~pollFirst;
            dial := FALSE;
            op := "connect";
            cancelled := FALSE;
            if sk = "idle" then                                              \* a fresh socket: under way,
                sk := "connecting";                                          \* and the operation waits in the kernel
                found := "wait";
            elsif sk = "connecting" then                                     \* dialled again, still connecting:
                found := "already";                                          \* EALREADY
                already := TRUE;
            elsif sk = "connected" then                                      \* connected meanwhile:
                found := "isconn";                                           \* EISCONN
            else                                                             \* refused meanwhile:
                found := "refused";                                          \* the socket's error
            end if;
        or                                                                   \* a ring: the poll for the socket to be writable
            await Backend = "ring" /\ pollFirst /\ op = "none";
            op := "poll";
            cancelled := FALSE;
        or                                                                   \* the connect operation ends:
            await op = "connect" /\ (found # "wait" \/ cancelled \/ Verdict # "none");
            op := "none";
            if cancelled /\ found = "wait" then                              \* ECANCELED, the handshake going on:
                \* MUTATION cancel_forgets_dial: the dial given back by the cancel is not kept.
                dial := Mutation # "cancel_forgets_dial";                    \* kept, and sent again
            elsif found = "wait" then                                        \* it waited for the decision:
                ended(Verdict);                                              \* and ends with it
            elsif found = "already" then                                     \* EALREADY: under way
                \* MUTATION ealready_error: the answer is taken as the connect's error.
                if Mutation = "ealready_error" then
                    ended("error");
                else
                    pollFirst := TRUE;                                       \* the side polls for the end
                end if;
            elsif found = "isconn" then                                      \* EISCONN:
                ended("ok");                                                 \* connected
            else                                                             \* refused:
                ended("refused");
            end if;
        or                                                                   \* the poll ends:
            await op = "poll" /\ (cancelled \/ Verdict # "none");
            op := "none";
            if ~cancelled then                                               \* the socket is writable:
                pollFirst := FALSE;
                if connecting then                                           \* a connect under way ended:
                    ended(Verdict);                                          \* the socket's error says how
                end if;
            end if;                                                          \* cancelled: polled again
        end either;
    end while;
end process;

\* A ring source moves to another shard once, while its operation is out: the move cancels it.
fair process mover = "mover"
begin
M_move:
    if Moves /\ Backend = "ring" then
        await op # "none" /\ ~cancelled;                                     \* an operation is out
        cancelled := TRUE;                                                   \* the move cancels it
        moved := TRUE;
    end if;
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

\* The caller hears its own socket's verdict: Ok only for a connection made, the refusal only for one
\* refused, and no other error.
OneResult == answered \in {"ok", "refused", "error"} => answered = Verdict

\* A caller that keeps polling hears its verdict.
Termination == <>(answered # "none")

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A dial sent again after a cancel finds the socket still connecting.
NoAlready == ~already
=============================================================================

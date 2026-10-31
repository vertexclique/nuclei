------------------------------- MODULE Connect -------------------------------
(***************************************************************************)
(* A Unix stream's connect to a listener whose backlog is full             *)
(* (src/net/unix.rs: `connect`; src/io/source/write.rs:                    *)
(* `WriteState::connect`, which forgets a connect that failed, so the next *)
(* poll makes another; src/io/sys/unix.rs: `connect_unix`).                *)
(*                                                                          *)
(* Who acts. A connector on the socket its caller set up, whose future the *)
(* caller may drop at any await. A listener with one place in its backlog, *)
(* which another connection took first; it accepts that one, freeing the   *)
(* place, or never accepts. A thread of the blocking pool, which only the  *)
(* old way used.                                                           *)
(*                                                                          *)
(* What can happen. Linux answers a non-blocking connect to a full backlog *)
(* with EAGAIN, and nothing tells the socket when a place frees. The       *)
(* connector waits a pause on a timer, then connects again on the same     *)
(* socket, until the listener takes the connection or answers another      *)
(* error, as std's blocking connect waits. The old way dropped the socket  *)
(* and made a blocking connect of a new one on a pool thread, which no     *)
(* drop of the future could stop.                                          *)
(*                                                                          *)
(* What must never happen. The caller hears EAGAIN; the stream it gets is  *)
(* another socket than the one it set up (its options lost); a connect     *)
(* says it connected while its socket is not in the listener's backlog; a  *)
(* pool thread stays held by a connect whose future was dropped.           *)
(*                                                                          *)
(* What is abstract. The pause and its growth to a cap are one step; the   *)
(* listener accepts once; macOS refuses a connect to a full backlog        *)
(* (ECONNREFUSED), as std's does there, which is an error like any other.  *)
(***************************************************************************)
EXTENDS Integers, TLC                                                        \* numbers and TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Accepts,   \* TRUE: the listener accepts the connection that took its place; FALSE: it never accepts
    Mutation   \* "none", or the one rule this run takes away

(* --algorithm Connect                                                        \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    backlog = 1,        \* connections in the listener's backlog, which has one place: another took it
    mine = FALSE,       \* the caller's own socket is in the backlog
    fresh = FALSE,      \* a socket the connect made itself is in the backlog
    answered = "none",  \* what the connect answered: "none" yet, "ok", "wouldblock", or "dropped" with its future
    returned = "none",  \* the socket the stream it answered holds: "none", "own" or "fresh"
    started = FALSE,    \* the write side's note that a connect of the socket was started
    retried = FALSE,    \* (record) the connect met the full backlog and connected again later
    pool = FALSE;       \* a thread of the blocking pool is held in a blocking connect

\* The connector: connects, and while the backlog is full pauses and connects again.
fair process connector = "connector"                                         \* the caller's connect
begin                                                                        \* the connector's steps:
C_loop:                                                                      \* one connect per step
    while answered = "none" do                                               \* until it answers
        either                                                               \* the caller drops the future:
            answered := "dropped";                                           \* and with it the socket
        or                                                                   \* a connect of the socket:
            if started then                                                  \* the side remembers one started:
                \* Only the no_restart mutation keeps that note past a failed connect: the side answers Ok
                \* without asking the kernel.
                answered := "ok";                                            \* as if it connected
                returned := "own";                                           \* its own socket
            elsif backlog < 1 then                                           \* a place is free:
                backlog := backlog + 1;                                      \* the kernel queues the connection
                mine := TRUE;                                                \* the caller's socket
                started := TRUE;                                             \* (the note)
                answered := "ok";                                            \* connected
                returned := "own";                                           \* on its own socket
            \* MUTATION no_retry: EAGAIN goes back to the caller.
            elsif Mutation = "no_retry" then                                 \* the backlog is full, and:
                answered := "wouldblock";                                    \* the caller hears it
            \* MUTATION pool: the old way, a blocking connect of a new socket on a pool thread.
            elsif Mutation = "pool" then                                     \* the backlog is full, and:
                pool := TRUE;                                                \* a pool thread blocks in it
                goto C_pool;                                                 \* the future waits for that thread
            \* MUTATION fresh_socket: the retry is made on a socket the connect made itself.
            elsif Mutation = "fresh_socket" then                             \* the backlog is full, and:
                goto C_fresh;                                                \* it waits for a place on a new socket
            else                                                             \* the backlog is full:
                \* The failed connect is forgotten, so the next poll makes another.
                \* MUTATION no_restart: the side keeps its note that the connect started.
                started := Mutation = "no_restart";                          \* (the mutation keeps it)
                retried := TRUE;                                             \* (record) it will connect again
            end if;                                                          \* one of the five
        end either;                                                          \* one of the two
    end while;                                                               \* answered
    goto C_done;                                                             \* done
C_fresh:                                                                     \* the fresh_socket mutation's wait
    either                                                                   \* a place comes:
        await backlog < 1;                                                   \* the listener freed it
        backlog := backlog + 1;                                              \* the new socket's connection is queued
        fresh := TRUE;                                                       \* not the caller's
        answered := "ok";                                                    \* connected
        returned := "fresh";                                                 \* on a socket without its options
    or                                                                       \* the caller drops the future:
        answered := "dropped";                                               \* the new socket with it
    end either;                                                              \* one of the two
    goto C_done;                                                             \* done
C_pool:                                                                      \* the pool mutation's wait
    either                                                                   \* the thread connects:
        await ~pool;                                                         \* its blocking connect ended
        answered := "ok";                                                    \* the stream comes back
        returned := "fresh";                                                 \* on the thread's socket
    or                                                                       \* the caller drops the future:
        answered := "dropped";                                               \* the thread blocks on all the same
    end either;                                                              \* one of the two
C_done:                                                                      \* the connector is done
    skip;                                                                    \* nothing more
end process;                                                                 \* end of the connector

\* The listener: accepts the connection that took its place, once, if it ever accepts.
fair process listener = "listener"                                           \* the far end
begin                                                                        \* the listener's step:
L_accept:                                                                    \* one accept
    if Accepts then                                                          \* it accepts:
        backlog := backlog - 1;                                              \* the place frees
    end if;                                                                  \* done
end process;                                                                 \* end of the listener

\* The pool thread of the old way: blocks in its connect until a place is free.
fair process thread = "thread"                                               \* a thread of the blocking pool
begin                                                                        \* the thread's steps:
P_wait:                                                                      \* it waits to be given a connect
    await pool \/ answered # "none";                                         \* given one, or the connect answered without
    if pool then                                                             \* given one:
P_connect:                                                                   \* its blocking connect
        await backlog < 1;                                                   \* a place frees
        backlog := backlog + 1;                                              \* its socket's connection is queued
        fresh := TRUE;                                                       \* not the caller's socket
        pool := FALSE;                                                       \* the thread is free again
    end if;                                                                  \* done
end process;                                                                 \* end of the thread
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* The caller never hears EAGAIN: a full backlog is waited out, as std's blocking connect waits.
NoWouldBlock == answered # "wouldblock"                                      \* never the kernel's EAGAIN

\* The stream the connect hands back is the socket its caller set up, with its options.
OwnSocket == returned # "fresh"                                              \* never another socket

\* A connect that answered Ok on its own socket has that socket in the listener's backlog.
OkMeansQueued == answered = "ok" /\ returned = "own" => mine                 \* never a connect that was not made

\* Once the caller dropped the future, no thread of the blocking pool is held for it.
FreedOnDrop == answered = "dropped" => ~pool                                 \* the drop frees everything

\* `Termination`, which the translation defines: with a listener that accepts, the connect answers and
\* every process ends.

(***************************************************************************)
(* Witness: claims an interesting case never happens and must be proved    *)
(* wrong.                                                                  *)
(***************************************************************************)

\* A connect meets the full backlog, pauses, and connects once a place frees.
NeverRetried == ~(retried /\ answered = "ok")                                \* broken by a listener that accepts
=============================================================================

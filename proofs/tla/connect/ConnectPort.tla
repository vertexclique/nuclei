----------------------------- MODULE ConnectPort -----------------------------
(***************************************************************************)
(* The local port a Windows connect binds to (src/io/sys/windows.rs        *)
(* `bind_any`, which `ConnectEx` needs before it connects; its caller is   *)
(* `NotStream::Connect` in src/io/source/port.rs `start`).                 *)
(*                                                                          *)
(* Who acts. A client connects again and again, each time to one of a few  *)
(* destinations, as a server's clients or a load test do. Each connection  *)
(* holds its local port while it is open and, once closed, while it waits  *)
(* out TIME_WAIT; the system lets those ports go in the end.               *)
(*                                                                          *)
(* What can happen. A connect binds its socket to port 0 first, and the    *)
(* system picks a port. With `SO_REUSE_UNICASTPORT` set before the bind,  *)
(* the system picks the port as a connect's own bind does (std's connect   *)
(* binds that way): any port not in use toward the same destination, so a  *)
(* port serves connections to many destinations. Without it, an explicit   *)
(* bind gets a port no other connection holds at all.                      *)
(*                                                                          *)
(* What must never happen. A connect fails for want of a port while a port *)
(* is free toward its destination: a connect std's would have made (the    *)
(* soak's clients ran out of ports so, WSAEADDRINUSE).                     *)
(*                                                                          *)
(* What is abstract. Ports and destinations are small numbers; a           *)
(* connection holds its pair until the system lets it go, with no time.    *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC                                            \* numbers, set sizes, TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Ports,      \* ephemeral ports the system has
    Dests,      \* destinations the client connects to
    Conns,      \* connects the client makes
    Mutation    \* "none", or the name of the one rule this run takes away

\* The pairs a port and a destination make.
Pairs == (1..Ports) \X (1..Dests)                                            \* every port toward every destination

(* --algorithm ConnectPort                                                     \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    held = {},             \* the <<port, destination>> pairs of connections open or in TIME_WAIT
    made = 0,              \* connects the client made
    failed = FALSE,        \* (record) a connect found no port
    failedAsStd = FALSE;   \* (record) a connect found no port while one was free toward its destination

define                                                                       \* names the steps and properties use:
    \* The ports free toward destination d, as a connect's own bind sees them.
    FreeToward(d) == {p \in 1..Ports : <<p, d>> \notin held}
    \* The ports no connection holds at all, as an explicit bind with no option sees them.
    FreeAtAll == {p \in 1..Ports : \A e \in 1..Dests : <<p, e>> \notin held}
    \* `bind_any` sets the option before its bind (mutated: it does not, as before).
    Reuse == Mutation # "no_reuse"
end define;                                                                  \* end of the names

\* The client: one connect after another, each to a destination it picks.
fair process client = "client"                                               \* weakly fair, like every process
begin                                                                        \* the client's steps:
C_connect:                                                                   \* one connect
    while made < Conns do                                                    \* while some are still to make
        with d \in 1..Dests do                                               \* toward a destination
            \* MUTATION no_reuse: the bind gets a port of its own, which no connection holds at all.
            with free = (IF Reuse THEN FreeToward(d) ELSE FreeAtAll) do      \* the ports the bind may get
                if free = {} then                                            \* none: WSAEADDRINUSE
                    failed := TRUE;                                          \* (record) the connect failed
                    failedAsStd := failedAsStd \/ FreeToward(d) # {};        \* (record) while std's had a port
                else                                                         \* one is picked:
                    with p \in free do                                       \* whichever the system picks
                        held := held \union {<<p, d>>};                      \* the connection holds it
                    end with;                                                \* until it is let go
                end if;                                                      \* the connect is over
            end with;                                                        \* the bind's choice made
        end with;                                                            \* toward that destination
        made := made + 1;                                                    \* one more connect
    end while;                                                               \* every connect made
end process;                                                                 \* end of the client

\* The system: a connection closed and through TIME_WAIT lets its port go, at some point.
fair process system = "system"                                               \* weakly fair: a port is let go in the end
begin                                                                        \* the system's steps:
S_release:                                                                   \* one let go
    while TRUE do                                                            \* for as long as the run goes
        await held # {};                                                     \* some pair is held
        with pair \in held do                                                \* whichever comes out of TIME_WAIT
            held := held \ {pair};                                           \* its port is free again
        end with;                                                            \* toward that destination
    end while;                                                               \* for as long as the run goes
end process;                                                                 \* end of the system
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* A connect fails for want of a port only where std's would: every port is in use toward its destination.
AsStd == ~failedAsStd                                                        \* never fails while std's would not

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A connect finds every port in use toward its destination, and fails as std's does.
NoFailure == ~failed                                                         \* never so
=============================================================================

--------------------------- MODULE AcceptStream ---------------------------
(***************************************************************************)
(* The stream of connections `TcpListener::accept_multi` gives             *)
(* (`TcpStreamGenerator::poll_next` in src/net/tcp.rs, `out_of_descriptors` *)
(* in src/rt/counters.rs), at the grain of its polls, the listener's       *)
(* answers and the ticks of its timer.                                     *)
(*                                                                          *)
(* Who acts. Peers connect, and their connections wait in the listener's   *)
(* backlog. The environment (the rest of the process) may take every free *)
(* descriptor for a while and give them back. A clock ticks while the     *)
(* stream's timer runs. The stream takes connections one poll at a time.  *)
(*                                                                          *)
(* What can happen. A poll asks the listener for the next connection: one  *)
(* in the backlog and a descriptor free, it is accepted and the stream's   *)
(* pause starts over at its least; none in the backlog, the poll waits for *)
(* one; the process out of descriptors (EMFILE or ENFILE), the connection  *)
(* stays in the backlog and the stream waits on a timer for its pause,    *)
(* which then doubles up to its most, and tries again when the timer      *)
(* fires; any other error ends the stream, and it accepts nothing more.   *)
(* The stream's side hands an error over once, and connections queued     *)
(* before it come first (`ReadState::accept_stream`), so a retry asks the *)
(* listener afresh.                                                        *)
(*                                                                          *)
(* What must never happen. A connection stays in the backlog for good once *)
(* descriptors are free again. The stream asks the listener again before   *)
(* its timer fired (a busy loop). An error other than running out of      *)
(* descriptors leaves the stream going. The first pause after an accept   *)
(* is longer than the least.                                               *)
(*                                                                          *)
(* What is abstract. Time is the ticks left on the timer (10 ms is one     *)
(* tick, so the pause doubles from 1 up to `MaxDelay`); the descriptors   *)
(* are one flag, out or not; the listener's other error comes once; a      *)
(* connection accepted is handed over and its descriptor goes back.       *)
(***************************************************************************)
EXTENDS Integers, TLC                                                        \* numbers and TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    N,          \* connections the peers make
    Outages,    \* times the environment takes every free descriptor
    MaxDelay,   \* the stream's longest pause, in ticks (its least is one)
    Other,      \* TRUE: the listener fails once with an error other than running out of descriptors
    Mutation    \* "none", or the name of the one rule this run takes away

MinDelay == 1                                                                \* the stream's least pause: one tick
Twice(d) == IF 2 * d > MaxDelay THEN MaxDelay ELSE 2 * d                     \* the next pause, doubled up to the most

(* --algorithm AcceptStream                                                    \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    backlog = 0,          \* connections waiting in the listener's backlog
    connected = 0,        \* connections the peers made
    out = FALSE,          \* the process has no descriptor left
    outages = 0,          \* times the descriptors ran out
    otherErr = FALSE,     \* the listener has an error other than running out of descriptors, not yet taken
    armed = FALSE,        \* the stream's timer runs
    left = 0,             \* ticks left on it
    delay = MinDelay,     \* the stream's next pause
    paused = FALSE,       \* the stream paused since its last accept
    tries = 0,            \* (ghost) times the listener said EMFILE since the last tick, at most two
    got = 0,              \* connections the stream handed over
    done = FALSE,         \* the stream ended
    badFirst = FALSE,     \* (record) a first pause after an accept was longer than the least
    retryServed = FALSE;  \* (record) an accept came after a pause

define                                                                       \* names the properties use:
    AllIn == connected = N                                                   \* every peer connected
end define;                                                                  \* end of the names

\* The peers: each connection joins the backlog.
fair process peers = "peers"                                                 \* weakly fair, like every process
begin                                                                        \* the peers' steps:
P_connect:                                                                   \* one connection
    while connected < N do                                                   \* while some are still to come
        backlog := backlog + 1;                                              \* it waits in the backlog
        connected := connected + 1;                                          \* and is counted
    end while;                                                               \* every peer connected
end process;                                                                 \* end of the peers

\* The environment: it takes every free descriptor a few times, and gives them back in the end (weakly fair).
fair process env = "env"                                                     \* weakly fair: descriptors come free again
begin                                                                        \* the environment's steps:
E_loop:                                                                      \* the next change
    while outages < Outages \/ out do                                        \* while an outage may come or lasts
        if out then out := FALSE;                                            \* descriptors free again
        else out := TRUE; outages := outages + 1;                            \* or none left
        end if;                                                              \* one change
    end while;                                                               \* no more outages
E_other:                                                                     \* the listener's other error
    if Other then otherErr := TRUE; end if;                                  \* it comes once, if this run has one
end process;                                                                 \* end of the environment

\* The clock: a tick while the stream's timer runs.
fair process clock = "clock"                                                 \* weakly fair: a running timer fires in the end
begin                                                                        \* the clock's steps:
C_tick:                                                                      \* one tick
    while TRUE do                                                            \* for as long as the stream runs
        await (armed /\ left > 0) \/ done;                                   \* a timer runs, or the stream ended
        if done then goto Done;                                              \* nothing more to time
        else left := left - 1; tries := 0;                                   \* a tick goes by
        end if;                                                              \* one tick
    end while;                                                               \* the stream ended
end process;                                                                 \* end of the clock

\* The stream (`TcpStreamGenerator::poll_next`).
fair process stream = "stream"                                               \* weakly fair, like every process
begin                                                                        \* the stream's steps:
S_loop:                                                                      \* the next poll
    while ~done do                                                           \* until it ends
        if armed then                                                        \* a pause under way:
            await left = 0;                                                  \* the timer fired (else the poll is Pending)
            armed := FALSE;                                                  \* and is done with
        end if;                                                              \* now the listener
S_accept:                                                                    \* `poll_accept_stream`
        if otherErr then                                                     \* another error:
            done := TRUE;                                                    \* the stream ends (`take_error` says why)
        elsif backlog > 0 /\ ~out then                                       \* a connection and a descriptor:
            backlog := backlog - 1;                                          \* accepted,
            got := got + 1;                                                  \* handed over,
            retryServed := retryServed \/ paused;                            \* (record) after a pause
            paused := FALSE;                                                 \* no pause since,
            \* MUTATION no_reset: the pause does not start over after an accept.
            if Mutation # "no_reset" then delay := MinDelay; end if;         \* and the pause starts over at its least
        elsif backlog > 0 then                                               \* no descriptor left (EMFILE, ENFILE):
            tries := IF tries < 2 THEN tries + 1 ELSE 2;                     \* (ghost) one more try this tick
            \* MUTATION end_on_emfile: the stream ends for good, as it did before.
            if Mutation = "end_on_emfile" then done := TRUE;                 \* (mutated) the backlog is left
            \* MUTATION no_backoff: the stream asks again at once, with no timer.
            elsif Mutation # "no_backoff" then                               \* a pause:
                badFirst := badFirst \/ (~paused /\ delay # MinDelay);         \* (record) a long first pause
                armed := TRUE;                                               \* the timer runs (`sleep(delay)`),
                left := delay;                                               \* for the pause,
                delay := Twice(delay);                                       \* the next one doubled,
                paused := TRUE;                                              \* a pause since the accept
            end if;                                                          \* the connection stays in the backlog
        else                                                                 \* nothing in the backlog:
            await backlog > 0 \/ otherErr;                                   \* the poll is Pending until one comes
        end if;                                                              \* the poll is over
    end while;                                                               \* the stream ended
end process;                                                                 \* end of the stream
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* Every connection in the backlog is accepted in the end, once descriptors are free again.
AllServed == <>(got = N)                                                     \* the whole backlog is served

\* No busy loop: the stream asks the listener again after EMFILE only once its timer fired, so at most one EMFILE comes
\* between two ticks.
NoBusyLoop == tries <= 1                                                     \* one try per tick at most

\* An error other than running out of descriptors ends the stream, and a stream that ended accepts nothing more.
OtherEnds == otherErr ~> done                                                \* the other error ends it
NothingAfterEnd == [][done => got' = got]_vars                               \* and it accepts nothing after

\* The first pause after an accept is the least.
FirstWaitLeast == ~badFirst                                                  \* never a long first pause

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A connection is accepted after the stream paused for running out of descriptors.
NoRetryServed == ~retryServed                                                \* never served after a pause
=============================================================================

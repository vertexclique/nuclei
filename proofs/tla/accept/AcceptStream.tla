--------------------------- MODULE AcceptStream ---------------------------
(***************************************************************************)
(* The stream of connections `TcpListener::accept_multi` gives             *)
(* (`TcpStreamGenerator::poll_next` in src/net/tcp.rs, `out_of_descriptors` *)
(* and `lost_before_accept` in src/rt/counters.rs), and the listener's      *)
(* accept under it: on a readiness poller `ReadState::accept_ahead` in      *)
(* src/io/source/read.rs, on a completion driver the accept's end in        *)
(* `ReadState::op_done` of src/io/source/read/ops.rs. At the grain of its   *)
(* polls, the listener's answers and the ticks of its timer.               *)
(*                                                                          *)
(* Who acts. Peers connect, and their connections wait in the listener's   *)
(* backlog; some peers reset theirs while it waits there. The environment *)
(* (the rest of the process) may take every free descriptor for a while   *)
(* and give them back. A clock ticks while the stream's timer runs. The   *)
(* stream takes connections one poll at a time.                           *)
(*                                                                          *)
(* What can happen. A poll asks the listener for the next connection: one  *)
(* in the backlog and a descriptor free, it is accepted and the stream's   *)
(* pause starts over at its least; one its peer reset, the listener        *)
(* answers with that connection's own error (ECONNABORTED on macOS and the *)
(* BSDs, WSAECONNRESET or ERROR_NETNAME_DELETED from `AcceptEx` on         *)
(* Windows), which the side skips, and the stream asks again at once; none *)
(* in the backlog, the poll waits for one; the process out of descriptors *)
(* (EMFILE or ENFILE), the stream waits on a timer for its pause, which    *)
(* then doubles up to its most, and tries again when the timer fires; any *)
(* other error ends the stream, and it accepts nothing more. Linux hands a *)
(* reset connection over as it is, its first read telling the reset: an    *)
(* accept like any other, which the connections the peers keep cover. On   *)
(* macOS an accept that fails for want of a descriptor closes the          *)
(* connection it took off the backlog (`Drops`), so after one the side     *)
(* first copies the listener's descriptor and closes the copy: no          *)
(* descriptor free, it answers EMFILE and takes nothing; one free, it      *)
(* accepts, though the rest of the process may take the last descriptor in *)
(* between. The stream's side hands an error over once, and connections    *)
(* queued before it come first (`ReadState::accept_stream`), so a retry    *)
(* asks the listener afresh.                                               *)
(*                                                                          *)
(* What must never happen. A connection stays in the backlog for good once *)
(* descriptors are free again. More connections are closed than the      *)
(* outages that began (one each at most: the first accept that met it).   *)
(* The stream asks the listener again before its timer fired (a busy      *)
(* loop). The stream ends for anything but the listener's own error: a    *)
(* reset connection or running out of descriptors. An error other than    *)
(* those leaves the stream going. The first pause after an accept is      *)
(* longer than the least.                                                  *)
(*                                                                          *)
(* What is abstract. Time is the ticks left on the timer (10 ms is one     *)
(* tick, so the pause doubles from 1 up to `MaxDelay`); the descriptors   *)
(* are one flag, out or not; the listener's other error comes once; a      *)
(* connection accepted is handed over and its descriptor goes back; the   *)
(* backlog is a count of each kind of connection, and any kind there may  *)
(* be at its head; a peer resets its connection before it joins it (one   *)
(* reset after it was taken is an accepted connection like any other).    *)
(***************************************************************************)
EXTENDS Integers, TLC                                                        \* numbers and TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    N,          \* connections the peers make
    Outages,    \* times the environment takes every free descriptor
    MaxDelay,   \* the stream's longest pause, in ticks (its least is one)
    Resets,     \* connections the peers make and reset while they wait in the backlog
    Other,      \* TRUE: the listener fails once with an error other than running out of descriptors
    Drops,      \* TRUE: an accept that fails for want of a descriptor closes the connection it took (macOS)
    Mutation    \* "none", or the name of the one rule this run takes away

MinDelay == 1                                                                \* the stream's least pause: one tick
Twice(d) == IF 2 * d > MaxDelay THEN MaxDelay ELSE 2 * d                     \* the next pause, doubled up to the most

(* --algorithm AcceptStream                                                    \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    backlog = 0,          \* connections waiting in the listener's backlog that their peers keep
    connected = 0,        \* connections the peers made and keep
    resets = 0,           \* connections waiting in the backlog that their peers reset
    resetMade = 0,        \* connections the peers made and reset
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
    retryServed = FALSE,  \* (record) an accept came after a pause
    short = FALSE,        \* the side's last accept failed for want of a descriptor: the next one checks first
    lost = 0,             \* connections a failed accept closed
    lostKept = 0,         \* those of them their peers kept
    raceLost = FALSE,     \* (record) a connection was closed by an accept whose check had found a descriptor free
    skipped = 0,          \* reset connections whose error the side skipped
    skipServed = FALSE,   \* (record) an accept came after a skip
    stalled = FALSE,      \* (mutated) a skip left the stream waiting for the next connection to come
    stallAt = 0;          \* how many connections the peers had made then

define                                                                       \* names the properties use:
    AllIn == connected = N                                                   \* every peer connected
    Made == connected + resetMade                                            \* connections the peers made, of each kind
    \* The kinds of connection that may be at the head of the backlog: one of each kind it holds.
    Heads == {k \in {"kept", "reset"} : IF k = "kept" THEN backlog > 0 ELSE resets > 0}
end define;                                                                  \* end of the names

\* The listener said EMFILE: the stream pauses on its timer, or (mutated) ends or asks again at once.
macro pause() begin                                                          \* the stream's answer to EMFILE
    tries := IF tries < 2 THEN tries + 1 ELSE 2;                             \* (ghost) one more try this tick
    \* MUTATION end_on_emfile: the stream ends for good, as it did before.
    if Mutation = "end_on_emfile" then done := TRUE;                         \* (mutated) the backlog is left
    \* MUTATION no_backoff: the stream asks again at once, with no timer.
    elsif Mutation # "no_backoff" then                                       \* a pause:
        badFirst := badFirst \/ (~paused /\ delay # MinDelay);               \* (record) a long first pause
        armed := TRUE;                                                       \* the timer runs (`sleep(delay)`),
        left := delay;                                                       \* for the pause,
        delay := Twice(delay);                                               \* the next one doubled,
        paused := TRUE;                                                      \* a pause since the accept
    end if;                                                                  \* the stream waits for its timer
end macro;                                                                   \* end of the answer

\* The peers: each connection joins the backlog, those that will be reset in any order with the others.
fair process peers = "peers"                                                 \* weakly fair, like every process
begin                                                                        \* the peers' steps:
P_connect:                                                                   \* one connection
    while connected < N \/ resetMade < Resets do                             \* while some are still to come
        with reset \in {r \in BOOLEAN : IF r THEN resetMade < Resets ELSE connected < N} do \* which kind comes next
            if reset then                                                    \* one its peer resets (zero linger):
                resets := resets + 1;                                        \* it waits in the backlog, reset
                resetMade := resetMade + 1;                                  \* and is counted
            else                                                             \* one its peer keeps:
                backlog := backlog + 1;                                      \* it waits in the backlog
                connected := connected + 1;                                  \* and is counted
            end if;                                                          \* one connection made
        end with;                                                            \* of the kind chosen
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
S_check:                                                                     \* `accept_ahead`, after a failed accept
        \* MUTATION no_probe: the side accepts at once, with no check first.
        if ~otherErr /\ short /\ Mutation # "no_probe" then                  \* it checks for a free descriptor:
            if out then                                                      \* the copy of the listener fails (EMFILE):
                pause();                                                     \* the stream pauses, the backlog untouched
                goto S_loop;                                                 \* and this poll is over
            end if;                                                          \* else the copy closes and the accept goes on
        end if;                                                              \* the rest of the process may act now
S_accept:                                                                    \* `poll_accept_stream`
        if otherErr then                                                     \* another error:
            done := TRUE;                                                    \* the stream ends (`take_error` says why)
        elsif stalled then                                                   \* (mutated) a skip left the poll waiting:
            await Made > stallAt;                                            \* Pending until another connection comes
            stalled := FALSE;                                                \* whose edge or completion asks again
        elsif Heads # {} /\ ~out then                                        \* a connection and a descriptor:
            with head \in Heads do                                           \* the connection at the head of the backlog:
                if head = "kept" then                                        \* one its peer keeps:
                    backlog := backlog - 1;                                  \* accepted,
                    got := got + 1;                                          \* handed over,
                    retryServed := retryServed \/ paused;                    \* (record) after a pause
                    skipServed := skipServed \/ (skipped > 0);               \* (record) after a skip
                    paused := FALSE;                                         \* no pause since,
                    short := FALSE;                                          \* the side checks no more,
                    \* MUTATION no_reset: the pause does not start over after an accept.
                    if Mutation # "no_reset" then delay := MinDelay; end if; \* and the pause starts over at its least
                else                                                         \* one its peer reset:
                    resets := resets - 1;                                    \* off the backlog and closed,
                    skipped := skipped + 1;                                  \* its error skipped (`lost_before_accept`),
                    short := FALSE;                                          \* a descriptor was free for it
                    \* MUTATION end_on_reset: the connection's error ends the stream, as WSAECONNRESET did on Windows.
                    if Mutation = "end_on_reset" then                        \* (mutated) the error is the stream's:
                        done := TRUE;                                        \* it ends, the backlog left
                    \* MUTATION skip_stalls: the skip asks the listener nothing more until another connection comes.
                    elsif Mutation = "skip_stalls" then                      \* (mutated) the poll waits:
                        stalled := TRUE;                                     \* for the next connection
                        stallAt := Made;                                     \* after those made so far
                    end if;                                                  \* else the next poll asks again at once
                end if;                                                      \* one connection off the backlog
            end with;                                                        \* whichever was at the head
        elsif Heads # {} then                                                \* no descriptor left (EMFILE, ENFILE):
            if Drops then                                                    \* macOS: the accept took a connection,
                raceLost := raceLost \/ short;                               \* (record) though its check found one free
                with head \in Heads do                                       \* whichever was at the head,
                    if head = "kept" then                                    \* one its peer kept:
                        backlog := backlog - 1;                              \* closed,
                        lostKept := lostKept + 1;                            \* and counted as such
                    else                                                     \* one its peer reset:
                        resets := resets - 1;                                \* closed
                    end if;                                                  \* one connection closed
                end with;                                                    \* off the backlog
                lost := lost + 1;                                            \* (counted)
            end if;                                                          \* elsewhere the connection stays
            \* MUTATION no_mark: the failed accept leaves the side checking nothing.
            if Mutation # "no_mark" then short := TRUE; end if;              \* the next accept checks first
            pause();                                                         \* the stream pauses
        else                                                                 \* nothing in the backlog:
            await Heads # {} \/ otherErr;                                    \* the poll is Pending until one comes
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

\* Every connection its peer keeps is accepted in the end, once descriptors are free again, or was closed by a failed
\* accept: none waits behind a reset one for good.
AllServed == <>(got + lostKept = N)                                          \* the whole backlog is served or closed

\* A failed accept closes at most one connection an outage: the one the first accept that met the outage took.
LostPerOutage == lost <= outages                                             \* no more closed than outages began

\* No busy loop: the stream asks the listener again after EMFILE only once its timer fired, so at most one EMFILE comes
\* between two ticks.
NoBusyLoop == tries <= 1                                                     \* one try per tick at most

\* An error other than running out of descriptors ends the stream, and a stream that ended accepts nothing more.
OtherEnds == otherErr ~> done                                                \* the other error ends it
NothingAfterEnd == [][done => got' = got]_vars                               \* and it accepts nothing after

\* Only the listener's own error ends the stream: never a connection its peer reset, nor running out of descriptors.
EndsOnlyForOther == done => otherErr                                         \* an end has the listener's error behind it

\* The first pause after an accept is the least.
FirstWaitLeast == ~badFirst                                                  \* never a long first pause

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A connection is accepted after the stream paused for running out of descriptors.
NoRetryServed == ~retryServed                                                \* never served after a pause

\* The rest of the process takes the last descriptor between a check that found one and the accept.
NoRaceLoss == ~raceLost                                                      \* never lost to that race

\* A connection is accepted after the side skipped one its peer reset.
NoSkipServes == ~skipServed                                                  \* never served after a skip
=============================================================================

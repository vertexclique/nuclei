---------------------------- MODULE AcceptReset ----------------------------
(***************************************************************************)
(* A listener's side when a peer resets its connection while it waits to  *)
(* be accepted, on every backend and through both ways of accepting:      *)
(* `accept` (and `incoming`, the stream of its results) and `accept_multi`. *)
(*                                                                          *)
(* Who acts. Peers connect into the backlog; some reset theirs (zero       *)
(* linger) at any time: in the backlog before an accept takes it, while an *)
(* accept that is out holds it, or after a multishot accept took it and    *)
(* before the task asks for its peer's address. The listener may fail once *)
(* with its own error (out of descriptors or memory, not listening). On a  *)
(* completion driver the kernel answers the accept out: IOCP's `AcceptEx`  *)
(* (`outcome` in src/io/source/port.rs, `status_error` in                  *)
(* src/syscore/windows/mod.rs) and io_uring's single accept               *)
(* (`RingMem::complete_rest` in src/io/source/ring.rs) alike, and          *)
(* io_uring's multishot accept (`ShardRing::accepted` in                   *)
(* src/rt/shard/ring/arrivals.rs). On a readiness poller (epoll, kqueue)   *)
(* the driver serves the listener's edges (`ReadState::serve` in           *)
(* src/io/source/read.rs). The task accepts: `Handle::accept` and          *)
(* `incoming` take each result (`ReadState::accept`, and `peer_gone` for   *)
(* a connection a multishot accept brought), `accept_multi`'s stream takes *)
(* connections (`ReadState::accept_stream`) and ends on an error           *)
(* (`TcpStreamGenerator::poll_next` in src/net/tcp.rs).                    *)
(*                                                                          *)
(* What can happen. An accept met by a connection its peer reset answers   *)
(* with that connection's error (ECONNABORTED from accept(2) on macOS and  *)
(* the BSDs; WSAECONNRESET or ERROR_NETNAME_DELETED from `AcceptEx`;       *)
(* ENOTCONN when the task asks a multishot accept's connection for its     *)
(* peer), or the kernel hands the connection over as it is (Linux), its    *)
(* first read telling the reset. The side tells such an error from the     *)
(* listener's own (`lost_before_accept` in src/rt/counters.rs) and skips   *)
(* it: a readiness side's loop takes the next connection at once           *)
(* (`ReadState::accept_ahead`); a completion side keeps nothing and wakes  *)
(* its task, whose poll sends the next accept (`ReadState::op_done` and    *)
(* `submit_ops` in src/io/source/read/ops.rs); a task that found a         *)
(* multishot accept's connection gone takes the next, the side accepting   *)
(* again when it holds none (`peer_gone`). The listener's own error is     *)
(* kept for the task: `accept` returns it, `accept_multi`'s stream ends    *)
(* with it.                                                                *)
(*                                                                          *)
(* What must never happen. A connection the peers keep waits for good      *)
(* while the task asks for connections. The task sees the error of a      *)
(* connection its peer reset, or a stream ends for one. The listener's own *)
(* error never reaches the task.                                           *)
(*                                                                          *)
(* What is abstract. The backlog and what the side holds are counts of     *)
(* each kind of connection, and any kind there may come first; one task    *)
(* accepts, so a readiness side takes one connection ahead at a time; the  *)
(* side's guard is one flag, which the task's poll, the driver's edge and *)
(* a completion each hold for their step (one that finds it held waits, as *)
(* the holder runs work deferred to it); running out of descriptors and    *)
(* the stream's pauses for it are `AcceptStream`'s; a plain accept on a    *)
(* ring that finds a multishot accept armed (by an earlier stream on the   *)
(* listener) takes what it brought, and sends single accepts once it ends. *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC                                             \* numbers, the procedures' stack, TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Kept,         \* connections the peers make and keep
    Doomed,       \* connections the peers make and reset, before, during or after an accept takes them
    ListenerErr,  \* TRUE: the listener fails once with its own error
    Backend,      \* "readiness" (epoll, kqueue), "completion" (IOCP, io_uring's single accept), "multishot"
    Form,         \* "accept" (`accept`, `incoming`) or "multi" (`accept_multi`)
    Mutation      \* "none", or the name of the one rule this run takes away

Kinds == {"kept", "doomed", "reset"}                                         \* kept; to be reset; reset already

(* --algorithm AcceptReset                                                     \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    keptQ = 0,            \* connections in the backlog their peers keep
    doomedQ = 0,          \* connections in the backlog their peers will reset
    resetQ = 0,           \* connections in the backlog their peers reset already
    keptMade = 0,         \* kept connections the peers made
    doomedMade = 0,       \* connections to reset the peers made
    listenerErr = FALSE,  \* the listener's own error waits for the next accept
    errCame = FALSE,      \* (record) the listener's error came
    out = (Backend = "multishot" /\ Form = "accept"),   \* an accept is out with the kernel (a stream armed one before)
    multi = (Backend = "multishot" /\ Form = "accept"), \* the accept out is a multishot one
    bound = "none",       \* the connection the kernel took for the single accept out, if any
    heldKept = 0,         \* kept connections the side holds for its task
    heldDoomed = 0,       \* connections to reset it holds
    heldReset = 0,        \* connections reset already it holds
    error = "none",       \* the error the side keeps for its task: "none", "listener", or (mutated) "lost"
    blocked = FALSE,      \* readiness: the last accept would have blocked, and no edge came since
    edge = FALSE,         \* readiness: the poller reported the listener, and the driver has not served it
    held = FALSE,         \* the side's guard is taken
    waiting = FALSE,      \* the task waits, its waker kept by the side
    got = 0,              \* kept connections the task took
    handed = 0,           \* reset connections handed over as they are (their first read tells the reset)
    skippedAt = {},       \* (record) where the side skipped a reset connection's error
    lostToCaller = FALSE, \* (record) the task saw a reset connection's error, or its stream ended for one
    surfaced = FALSE,     \* the listener's error reached the task
    ended = FALSE;        \* the stream ended (`accept_multi`)

define                                                                       \* names the steps and properties use:
    \* `lost_before_accept`: the error is a reset connection's, which the side skips (mutated: the listener's too).
    Lost(answer) == answer = "lost" \/ (answer = "listener" /\ Mutation = "skip_listener")
    Held == heldKept + heldDoomed + heldReset                                \* connections the side holds
    HasResult == Held > 0 \/ error # "none"                                  \* the side has something for its task
    \* The kinds of connection that may be at the head of the backlog: one of each kind it holds.
    Heads == {k \in Kinds : CASE k = "kept" -> keptQ > 0 [] k = "doomed" -> doomedQ > 0 [] OTHER -> resetQ > 0}
    Ask == Backend = "multishot"                                             \* held connections have no address yet
    \* The one place whose skip a mutation takes away: "ahead" (`accept_ahead`), "op" (`op_done` of an accept),
    \* "multiEnd" (`op_done` of a multishot accept's end), "peer" (`peer_gone`), or "none".
    KeepAt == CASE Mutation = "keep_lost_ahead" -> "ahead"                   \* keeps the error in the loop
              [] Mutation = "keep_lost_op" -> "op"                           \* in a completed accept
              [] Mutation = "keep_lost_end" -> "multiEnd"                    \* in a multishot accept's end
              [] Mutation = "keep_lost_peer" -> "peer"                       \* in a look at a connection's peer
              [] OTHER -> "none"                                             \* nowhere
    StallAt == CASE Mutation = "stall_ahead" -> "ahead"                      \* stalls the loop
               [] Mutation = "stall_op" -> "op"                              \* a completed accept
               [] Mutation = "stall_end" -> "multiEnd"                       \* a multishot accept's end
               [] Mutation = "stall_peer" -> "peer"                          \* a look at a connection's peer
               [] OTHER -> "none"                                            \* nowhere
end define;                                                                  \* end of the names

\* `ReadState::op_done`: the accept out ended with `answer`, a connection or an error, met at `at`.
macro op_done(answer, at) begin                                              \* the completion, under the side's guard
    out := FALSE;                                                            \* no accept is out now
    multi := FALSE;                                                          \* nor a multishot one
    if answer = "kept" then heldKept := heldKept + 1;                        \* a kept connection, for the task
    elsif answer = "doomed" then heldDoomed := heldDoomed + 1;               \* one reset only after it was taken
    elsif answer = "reset" then heldReset := heldReset + 1;                  \* one handed over though reset
    elsif Lost(answer) then                                                  \* a reset connection's error:
        skippedAt := skippedAt \union {at};                                  \* (record) skipped here
        \* MUTATION keep_lost_op, keep_lost_end: the side keeps the reset connection's error for its task, as before.
        if KeepAt = at then error := answer; end if;                         \* (mutated) kept for the task
    else                                                                     \* the listener's own error:
        error := answer;                                                     \* kept for the task
    end if;                                                                  \* what came is in place
    \* MUTATION stall_op, stall_end: a skip wakes nobody, so no accept goes out for the task that waits.
    if ~(Lost(answer) /\ StallAt = at) then waiting := FALSE; end if;       \* the task is woken
end macro;                                                                   \* end of the completion

\* `ReadState::accept_ahead`: a readiness side accepts until it holds a connection, an error ends it, or the listener
\* would block. A reset connection's error is skipped and the next connection taken at once.
procedure acceptAhead()                                                      \* called under the side's guard
variable stop = FALSE;                                                       \* the loop ends at this call
begin                                                                        \* the loop's steps:
AA_loop:                                                                     \* one accept(2)
    while Held = 0 /\ ~stop do                                               \* `while self.accepted.len() < limit`
        if Heads = {} /\ ~listenerErr then                                   \* nothing to accept:
            blocked := TRUE;                                                 \* EAGAIN, until the next edge
            stop := TRUE;                                                    \* the loop ends
        else                                                                 \* the listener answers:
            with answer \in Heads \union (IF listenerErr THEN {"listener"} ELSE {}) do \* whichever comes first
                if answer = "kept" then                                      \* a kept connection:
                    keptQ := keptQ - 1;                                      \* off the backlog
                    heldKept := heldKept + 1;                                \* held for the task
                elsif answer = "doomed" then                                 \* one its peer resets after this call:
                    doomedQ := doomedQ - 1;                                  \* off the backlog
                    heldDoomed := heldDoomed + 1;                            \* held for the task
                elsif answer = "listener" then                               \* the listener's own error:
                    listenerErr := FALSE;                                    \* answered once
                    if Lost("listener") then                                 \* (mutated) taken for a reset one's:
                        skippedAt := skippedAt \union {"ahead"};             \* skipped, and lost
                    else                                                     \* as it is:
                        error := "listener";                                 \* kept for the task
                        stop := TRUE;                                        \* and the loop ends
                    end if;                                                  \* the error is placed
                else                                                         \* one its peer reset already:
                    resetQ := resetQ - 1;                                    \* off the backlog
                    either                                                   \* Linux hands it over as it is:
                        heldReset := heldReset + 1;                          \* held for the task
                    or                                                       \* macOS, the BSDs: ECONNABORTED
                        skippedAt := skippedAt \union {"ahead"};             \* (record) skipped here
                        \* MUTATION keep_lost_ahead: the loop keeps the reset connection's error, as before.
                        if KeepAt = "ahead" then                             \* (mutated) kept for the task:
                            error := "lost";                                 \* the reset one's error
                            stop := TRUE;                                    \* ends the loop
                        \* MUTATION stall_ahead: the skip ends the loop with nothing held, no error, and not blocked.
                        elsif StallAt = "ahead" then                         \* (mutated) the loop gives up:
                            stop := TRUE;                                    \* until an edge that may never come
                        end if;                                              \* else the next accept at once
                    end either;                                              \* the reset one is dealt with
                end if;                                                      \* one answer taken
            end with;                                                        \* whichever came
        end if;                                                              \* one call made
    end while;                                                               \* the loop ended
AA_end:                                                                      \* back to the caller
    return;                                                                  \* the task's poll or the driver's edge
end procedure;                                                               \* end of the loop

\* The peers: each connection joins the backlog, those to be reset in any order with the kept ones.
fair process peers = "peers"                                                 \* weakly fair, like every process
begin                                                                        \* the peers' steps:
P_connect:                                                                   \* one connection
    while keptMade < Kept \/ doomedMade < Doomed do                          \* while some are still to come
        with doomed \in {d \in BOOLEAN : IF d THEN doomedMade < Doomed ELSE keptMade < Kept} do \* which kind next
            if doomed then                                                   \* one its peer will reset:
                doomedQ := doomedQ + 1;                                      \* it waits in the backlog
                doomedMade := doomedMade + 1;                                \* and is counted
            else                                                             \* one its peer keeps:
                keptQ := keptQ + 1;                                          \* it waits in the backlog
                keptMade := keptMade + 1;                                    \* and is counted
            end if;                                                          \* one connection made
        end with;                                                            \* of the kind chosen
        if Backend = "readiness" then edge := TRUE; end if;                  \* the poller reports it
    end while;                                                               \* every peer connected
end process;                                                                 \* end of the peers

\* The peers' resets: a connection to be reset is reset in the backlog, under the single accept out that took it, or,
\* where the side holds connections with no address yet, after a multishot accept took it.
fair process resetter = "resetter"                                           \* weakly fair: a reset comes in the end
begin                                                                        \* the resets' steps:
R_reset:                                                                     \* one reset
    while TRUE do                                                            \* whenever one is still to come
        either                                                               \* before any accept took it:
            await doomedQ > 0;                                               \* one in the backlog,
            doomedQ := doomedQ - 1;                                          \* no longer to be reset,
            resetQ := resetQ + 1;                                            \* but reset
        or                                                                   \* during the accept:
            await bound = "doomed";                                          \* the one the accept out took
            bound := "reset";                                                \* is reset before it completes
        or                                                                   \* after a multishot accept took it:
            await Ask /\ heldDoomed > 0;                                     \* one the side holds, with no address
            heldDoomed := heldDoomed - 1;                                    \* no longer to be reset,
            heldReset := heldReset + 1;                                      \* but reset before the task asks its peer
        end either;                                                          \* one reset
    end while;                                                               \* for as long as the run goes
end process;                                                                 \* end of the resets

\* The listener's own error: it comes once, if this run has one.
fair process env = "env"                                                     \* weakly fair: the error comes in the end
begin                                                                        \* the error's step:
E_error:                                                                     \* the listener fails
    if ListenerErr then                                                      \* in a run that has the error:
        listenerErr := TRUE;                                                 \* the next accept answers it
        errCame := TRUE;                                                     \* (record) it came
        if Backend = "readiness" then edge := TRUE; end if;                  \* with an edge, as the backlog's own
    end if;                                                                  \* or never
end process;                                                                 \* end of the error

\* The kernel of a completion driver: it answers the accept out.
fair process kernel = "kernel"                                               \* weakly fair: what is out completes
begin                                                                        \* the kernel's steps:
K_answer:                                                                    \* one answer
    while TRUE do                                                            \* for as long as the run goes
        either                                                               \* the single accept takes a connection:
            await out /\ ~multi /\ bound = "none" /\ Heads # {};             \* one in the backlog
            with k \in Heads do                                              \* the one at the head
                if k = "kept" then keptQ := keptQ - 1;                       \* off the backlog,
                elsif k = "doomed" then doomedQ := doomedQ - 1;              \* whichever kind
                else resetQ := resetQ - 1;                                   \* it is,
                end if;                                                      \* gone from there
                bound := k;                                                  \* and the accept's
            end with;                                                        \* until it completes
        or                                                                   \* it completes with a live connection:
            await out /\ ~multi /\ bound \in {"kept", "doomed"} /\ ~held;    \* the side's guard free
            op_done(bound, "op");                                            \* `Outcome::Accept(Done(Ok))`
            bound := "none";                                                 \* nothing taken now
        or                                                                   \* it completes with a reset one:
            await out /\ ~multi /\ bound = "reset" /\ ~held;                 \* the side's guard free
            either                                                           \* with the reset's error
                op_done("lost", "op");                                       \* (WSAECONNRESET, ERROR_NETNAME_DELETED)
            or                                                               \* or as it is
                op_done("reset", "op");                                      \* (its first read tells it)
            end either;                                                      \* one completion
            bound := "none";                                                 \* nothing taken now
        or                                                                   \* it completes with the listener's error:
            await out /\ ~multi /\ bound = "none" /\ listenerErr /\ ~held;   \* the side's guard free
            listenerErr := FALSE;                                            \* answered once
            op_done("listener", "op");                                       \* `Outcome::Accept(Done(Err))`
        or                                                                   \* the multishot accept brings one:
            await out /\ multi /\ Heads # {} /\ ~held;                       \* the side's guard free
            with k \in Heads do                                              \* the one at the head, as an arrival
                if k = "kept" then                                           \* kept:
                    keptQ := keptQ - 1;                                      \* off the backlog
                    heldKept := heldKept + 1;                                \* held, with no address
                elsif k = "doomed" then                                      \* to be reset:
                    doomedQ := doomedQ - 1;                                  \* off the backlog
                    heldDoomed := heldDoomed + 1;                            \* held, with no address
                else                                                         \* reset already:
                    resetQ := resetQ - 1;                                    \* off the backlog
                    heldReset := heldReset + 1;                              \* held as it is
                end if;                                                      \* one arrival
            end with;                                                        \* the multishot stays armed
            waiting := FALSE;                                                \* the task is woken (`take_arrival`)
        or                                                                   \* it ends with a reset one's error:
            await out /\ multi /\ resetQ > 0 /\ ~held;                       \* the side's guard free
            resetQ := resetQ - 1;                                            \* that connection gone
            op_done("lost", "multiEnd");                                     \* `Outcome::AcceptMultiEnd(Done(Err))`
        or                                                                   \* it ends with the listener's error:
            await out /\ multi /\ listenerErr /\ ~held;                      \* the side's guard free
            listenerErr := FALSE;                                            \* answered once
            op_done("listener", "multiEnd");                                 \* the end, with the error
        end either;                                                          \* one answer
    end while;                                                               \* for as long as the run goes
end process;                                                                 \* end of the kernel

\* The driver of a readiness poller: an edge clears `blocked`, and for a task that waits the side accepts and wakes it
\* if it then holds something (`ReadState::serve`).
fair process driver = "driver"                                               \* weakly fair: an edge is served
begin                                                                        \* the driver's steps:
D_edge:                                                                      \* one edge
    while Backend = "readiness" do                                           \* only a readiness poller has edges
        await edge /\ ~held;                                                 \* an edge, and the side's guard free
        held := TRUE;                                                        \* the guard is the driver's
        edge := FALSE;                                                       \* the edge is served
        blocked := FALSE;                                                    \* the listener may hold something
D_call:                                                                      \* `complete_fd`
        if waiting /\ ~HasResult then                                        \* for a task that waits:
            call acceptAhead();                                              \* the side accepts
        end if;                                                              \* or nothing to do
D_wake:                                                                      \* the task
        if waiting /\ HasResult then waiting := FALSE; end if;               \* is woken for what came
        held := FALSE;                                                       \* and the guard let go
    end while;                                                               \* the next edge
end process;                                                                 \* end of the driver

\* The task: `accept` again and again (a server's loop), or `accept_multi`'s stream until it ends.
fair process task = "task"                                                   \* weakly fair, like every process
begin                                                                        \* the task's steps:
T_loop:                                                                      \* one poll
    while ~ended do                                                          \* until the stream ends
        await ~waiting;                                                      \* a task that waits is woken first
T_hold:                                                                      \* the poll takes the side
        await ~held;                                                         \* its guard free
        held := TRUE;                                                        \* and the task's
T_complete:                                                                  \* `complete`: the driver's part first
        if Backend = "readiness" then                                        \* a readiness side
            if ~blocked /\ ~HasResult then                                   \* that may find something
                call acceptAhead();                                          \* accepts
            end if;                                                          \* or holds what it has
        elsif ~out /\ ~HasResult then                                        \* a completion side with nothing out:
            out := TRUE;                                                     \* `submit_ops` sends an accept,
            multi := Backend = "multishot" /\ Form = "multi";                \* multishot for a ring's stream
        end if;                                                              \* the side asked
T_take:                                                                      \* `ReadState::accept`, `accept_stream`
        either                                                               \* a kept connection:
            await heldKept > 0;                                              \* the side holds one
            heldKept := heldKept - 1;                                        \* taken
            got := got + 1;                                                  \* by the task
            held := FALSE;                                                   \* the guard let go
        or                                                                   \* one its peer resets after:
            await heldDoomed > 0;                                            \* the side holds one
            heldDoomed := heldDoomed - 1;                                    \* taken
            handed := handed + 1;                                            \* by the task, as it is
            held := FALSE;                                                   \* the guard let go
        or                                                                   \* one reset already, its address known:
            await heldReset > 0 /\ ~(Ask /\ Form = "accept");                \* a stream asks no address
            heldReset := heldReset - 1;                                      \* taken
            handed := handed + 1;                                            \* by the task, as it is
            held := FALSE;                                                   \* the guard let go
        or                                                                   \* one a multishot accept brought, reset:
            await heldReset > 0 /\ Ask /\ Form = "accept";                   \* `peer_of` says ENOTCONN
            heldReset := heldReset - 1;                                      \* it is closed
            skippedAt := skippedAt \union {"peer"};                          \* (record) skipped here (`peer_gone`)
            \* MUTATION keep_lost_peer: the look returns the reset connection's error to the task, as before.
            if KeepAt = "peer" then                                          \* (mutated) the error to the task:
                lostToCaller := TRUE;                                        \* as before
                held := FALSE;                                               \* the guard let go
            \* MUTATION stall_peer: the skip takes from what is held, and sends no accept for an empty side.
            elsif StallAt = "peer" then                                      \* (mutated) no accept first:
                goto T_take;                                                 \* only what is held
            else                                                             \* `peer_gone`:
                goto T_complete;                                             \* the side accepts again, then the next
            end if;                                                          \* the reset one is dealt with
        or                                                                   \* an error, and nothing held:
            await Held = 0 /\ error # "none";                                \* the side kept one
            if error = "lost" then lostToCaller := TRUE; end if;             \* (mutated) a reset one's, to the task
            if error = "listener" then surfaced := TRUE; end if;             \* the listener's, to the task
            if Form = "multi" then ended := TRUE; end if;                    \* a stream ends (`take_error` says why)
            error := "none";                                                 \* handed over once
            held := FALSE;                                                   \* the guard let go
        or                                                                   \* nothing yet:
            await Held = 0 /\ error = "none";                                \* the side holds nothing
            waiting := TRUE;                                                 \* the task waits, its waker kept
            held := FALSE;                                                   \* the guard let go
        end either;                                                          \* the poll is over
    end while;                                                               \* the stream ended
end process;                                                                 \* end of the task
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* Every connection its peer keeps is taken by the task in the end: none waits behind a reset one for good.
AllKeptServed == <>(got = Kept)                                              \* every kept connection served

\* The task never sees a reset connection's error, and no stream ends for one.
NoLostToCaller == ~lostToCaller                                              \* never a reset one's error

\* The listener's own error reaches the task in the end: `accept` returns it, the stream ends with it.
ErrSurfaces == errCame ~> surfaced                                           \* the error is never swallowed

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* Every kept connection is served after a readiness side skipped a reset one in its loop.
NoServedAfterAheadSkip == ~("ahead" \in skippedAt /\ got = Kept)             \* never so
\* Every kept connection is served after a completion side skipped a reset one's accept.
NoServedAfterOpSkip == ~("op" \in skippedAt /\ got = Kept)                   \* never so
\* Every kept connection is served after a multishot accept ended with a reset one's error.
NoServedAfterEndSkip == ~("multiEnd" \in skippedAt /\ got = Kept)            \* never so
\* Every kept connection is served after the task found a multishot accept's connection gone.
NoServedAfterPeerSkip == ~("peer" \in skippedAt /\ got = Kept)               \* never so
=============================================================================

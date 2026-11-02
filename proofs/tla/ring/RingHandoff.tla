---------------------------- MODULE RingHandoff ----------------------------
(***************************************************************************)
(* A worker's ring handed to another thread while an operation its old     *)
(* runner submitted waits in the kernel (src/rt/shard/ring.rs:             *)
(* `ShardRing::adopt`, which the new runner calls before its first drive,  *)
(* `run_shard` in src/rt/workers.rs; `RingPoller::adopt` in               *)
(* src/rt/park/ring.rs for the ring's wake; the zombie's later completions *)
(* in `ShardRing::complete`, `ShardRing::accepted` in                      *)
(* src/rt/shard/ring/arrivals.rs and `ShardRing::bury`; the ring's setup   *)
(* flags, `setup_flags` in src/syscore/linux/iouring/ring/mod.rs).         *)
(*                                                                          *)
(* Who acts. The old runner, the thread the monitor took the shard from,   *)
(* blocked in a task's poll; the new runner, which runs the shard and its  *)
(* ring from now on; the kernel, which runs a request's completion as work *)
(* queued to the thread that submitted it (a watched worker's ring is not *)
(* single-issuer, so its task work is not deferred to the ring's waiter);  *)
(* a peer, which sends one datum on the socket.                           *)
(*                                                                          *)
(* What can happen. The old runner armed a request before it blocked: a    *)
(* poll (a stream read by readiness, a datagram's poll, a poll for room,  *)
(* the ring's own wake read: what it brings late is only an edge or a     *)
(* wake), a data operation (a receive, a send: what it takes or gives     *)
(* cannot be done twice), or an accept, whose poll in the kernel is an    *)
(* exclusive waiter of its listener: a connection's wake goes to the      *)
(* first such waiter alone, which takes it even once cancelled, until its *)
(* own thread ends it. The datum's arrival queues the request's           *)
(* completion to the old runner's thread. A thread in an interruptible    *)
(* wait is woken to run it; one in a wait no signal ends (a killable wait, *)
(* a disk read) runs it only once it comes back. So the new runner,       *)
(* before anything else, cancels each poll the old runner armed, tells    *)
(* its side that poll ended (the side then arms another, submitted by the *)
(* new runner, whose completion runs on the new runner), and keeps the old *)
(* one as a zombie: whatever the zombie still posts, whenever its old     *)
(* thread runs it, goes nowhere, and its last completion only frees it.   *)
(* A data operation and an accept are left as they are: their completions *)
(* wait for the old runner.                                                *)
(*                                                                          *)
(* What must never happen. The reader never gets the datum (`IoRescued`), *)
(* the guard against a ring whose task work waits for its thread's next   *)
(* enter (`IORING_SETUP_COOP_TASKRUN`) included: the new runner sleeps in  *)
(* its wait and would never run its own requests' completions. The side   *)
(* has two requests out as its own (`OneOut`), or takes a completion of a  *)
(* request it no longer has out (`OwnEnds`).                               *)
(*                                                                          *)
(* What is abstract. One socket side, one datum, one take (the monitor's  *)
(* take is the model's start: proofs/tla/handoff checks it). A multishot  *)
(* request stays armed across its edges; a data operation is one shot.    *)
(* The cancel marks the request at once and queues its completion to its *)
(* own submitter, as the kernel does for a poll it cancels.               *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC                                 \* numbers, sequences (the completion queue), set sizes, TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Block,      \* how the old runner waits: "interruptible" (a signal wakes it) or "uninterruptible"
    Ends,       \* TRUE: the old runner's wait ends in the end, and it comes back
    Kind,       \* the request the old runner armed: "poll" (readiness, the wake), "data" or "accept"
    Mutation    \* "none", or the name of the one rule this run takes away

Old == "W1"                                                                  \* the old runner
New == "W2"                                                                  \* the new runner
Reqs == 1..2                                                                 \* request 1, the old runner's; 2, the new runner's

(* --algorithm RingHandoff
variables                                                                    \* the shared state:
    sub = [r \in Reqs |-> IF r = 1 THEN Old ELSE New],   \* who submitted each request
    live = [r \in Reqs |-> r = 1],          \* the request is in the kernel: only the old one at the start
    cancelled = [r \in Reqs |-> FALSE],     \* a cancel reached the request
    zombie = [r \in Reqs |-> FALSE],        \* the request is no side's any more
    tw = [t \in {Old, New} |-> {}],         \* the requests whose completion work is queued to each thread
    cq = <<>>,                              \* the completion queue: <<request, what>>
    data = 0,                               \* the datum waiting in the socket
    sent = FALSE,                           \* the peer sent it
    delivered = FALSE,                      \* the reader got it
    out = 1,                                \* the request the side has out as its own (0: none)
    back = FALSE,                           \* the old runner came back from its wait
    oldPosted = FALSE,                      \* (record) a completion of the old request was reaped after the take
    stray = FALSE,                          \* the side took a completion of a request it does not have out
    buried = FALSE;                         \* the zombie's last completion freed it

define                                                                       \* names the properties use:
    \* The side takes only the completions of the request it has out.
    OwnEnds == ~stray                                                        \* never another's
    \* The side has at most one request out as its own.
    OneOut == Cardinality({r \in Reqs : live[r] /\ ~zombie[r]}) <= 1         \* one at most
    \* The reader gets the datum (an accept, its connection) in the end.
    IoRescued == <>delivered                                                 \* eventually delivered
    \* The zombie is freed in the end.
    ZombieFreed == <>(zombie[1] => buried)                                   \* a zombie is buried in the end
    \* Witness: a completion of the old request is reaped after the take.
    NoOldPost == ~oldPosted                                                  \* claimed never to happen
    \* Whether thread `t` runs its queued completion work now.
    Runs(t) == IF t = Old                                                    \* the old runner:
               THEN Block = "interruptible" \/ back                          \* woken by a signal, or back
               ELSE Mutation # "coop_taskrun" \/ cq # <<>>                   \* the new one: woken by the work, or (coop) only by a completion
end define;                                                                  \* end of the names

\* The kernel's completion work for a request: the datum's edge for a poll, the datum for a data operation or an
\* accept, the end of a cancelled one.
macro complete(r) begin                                                      \* its steps:
    if cancelled[r] then                                                     \* cancelled:
        live[r] := FALSE;                                                    \* it leaves the kernel,
        cq := Append(cq, <<r, "end">>);                                      \* with its last completion
    elsif Kind = "poll" then                                                 \* a poll:
        cq := Append(cq, <<r, "edge">>);                                     \* an edge, and it stays armed
    elsif data > 0 then                                                      \* a data operation or an accept, with the datum there:
        data := 0;                                                           \* it takes it,
        live[r] := FALSE;                                                    \* ends,
        cq := Append(cq, <<r, "data">>);                                     \* and posts it
    end if;                                                                  \* (no datum: nothing to do)
end macro;                                                                   \* end of the completion work

\* The peer: it sends one datum (a connection, for an accept), and the kernel wakes the requests waiting on the
\* socket: every one, or for an accept only the first still in the kernel, which takes the wake even once cancelled.
fair process peer = "P"                                                      \* weakly fair, like every process
begin                                                                        \* the peer's step:
P_send:                                                                      \* the datum arrives
    data := 1;                                                               \* in the socket
    sent := TRUE;                                                            \* sent
    with woken = IF Kind = "accept"                                          \* who the wake reaches:
                 THEN {r \in Reqs : live[r] /\ \A q \in Reqs : live[q] => r <= q}   \* the first waiter alone,
                 ELSE {r \in Reqs : live[r]} do                               \* or every one
        tw := [t \in {Old, New} |->                                          \* each thread's work grows by
                 tw[t] \cup {r \in woken : ~cancelled[r] /\ sub[r] = t}];   \* its woken requests a cancel did not already
    end with;                                                                \* (a cancelled one's work is queued already)
end process;                                                                 \* end of the peer

\* The old runner: blocked in a task's poll, its queued work run as its wait allows.
fair process old = Old                                                       \* weakly fair, like every process
begin                                                                        \* the old runner's steps:
W1_loop:                                                                     \* its work, and its way back
    while TRUE do                                                            \* for good
        either                                                               \* its queued completion work runs:
            await Runs(Old) /\ tw[Old] # {};                                 \* while its wait allows
            with r \in tw[Old] do                                            \* one request's
                tw[Old] := tw[Old] \ {r};                                    \* taken
                complete(r);                                                 \* and done
            end with;                                                        \* the next
        or                                                                   \* or its wait ends:
            await Ends /\ ~back;                                             \* where it does
            back := TRUE;                                                    \* it came back
        end either;                                                          \* the next
    end while;                                                               \* forever
end process;                                                                 \* end of the old runner

\* The new runner: the adopt, then its ring.
fair process new = New                                                       \* weakly fair, like every process
begin                                                                        \* the new runner's steps:
W2_adopt:                                                                    \* `ShardRing::adopt`, before its first drive
    \* MUTATION no_adopt: the new runner leaves the old runner's requests as they are, as before the fix.
    \* MUTATION adopt_accepts: the new runner cancels and arms again an accept too.
    if Mutation # "no_adopt" /\ live[1]                                      \* a request of the old runner's that is
       /\ (Kind = "poll" \/ (Kind = "accept" /\ Mutation = "adopt_accepts")) then   \* a poll:
        cancelled[1] := TRUE;                                                \* cancelled from here,
        \* MUTATION two_out: the side is not told the old request ended, and has both out as its own.
        if Mutation # "two_out" then zombie[1] := TRUE; end if;              \* no side's any more,
        out := 2;                                                            \* and the side, told it ended, arms another, submitted here
        live[2] := TRUE;                                                     \* in the kernel
        tw := [tw EXCEPT ![Old] = @ \cup {1},                                \* the cancel queues the old one's end to its own thread
                         ![New] = IF data > 0 THEN @ \cup {2} ELSE @];       \* armed on a ready socket: its edge comes at once
    end if;                                                                  \* (a data operation or an accept is left as it is)
W2_loop:                                                                     \* the ring's waits
    while TRUE do                                                            \* for good
        either                                                               \* its own queued completion work runs:
            await Runs(New) /\ tw[New] # {};                                 \* as its wait allows
            with r \in tw[New] do                                            \* one request's
                tw[New] := tw[New] \ {r};                                    \* taken
                complete(r);                                                 \* and done
            end with;                                                        \* the next
        or                                                                   \* or a completion is reaped:
            await cq # <<>>;                                                 \* one is there
            if Head(cq)[1] = 1 then oldPosted := TRUE; end if;               \* (record) the old request's
            \* MUTATION zombie_reaches_side: the zombie's completions are handed to the side as its own.
            if zombie[Head(cq)[1]] /\ Mutation # "zombie_reaches_side" then  \* a zombie's (`ShardRing::bury` at its last):
                if Head(cq)[2] = "end" then buried := TRUE; end if;          \* its last frees it; the rest goes nowhere
            else                                                             \* the side's own request's:
                if Head(cq)[1] # out then stray := TRUE; end if;             \* (a request the side does not have out)
                if Head(cq)[2] = "edge" then                                 \* an edge:
                    if data > 0 then data := 0; delivered := TRUE; end if;   \* the reader reads the datum itself
                elsif Head(cq)[2] = "data" then                              \* the datum:
                    delivered := TRUE;                                       \* the reader has it
                end if;                                                      \* the completion is done
            end if;                                                          \* the side took it
            cq := Tail(cq);                                                  \* reaped
        end either;                                                          \* the next
    end while;                                                               \* forever
end process;                                                                 \* end of the new runner
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

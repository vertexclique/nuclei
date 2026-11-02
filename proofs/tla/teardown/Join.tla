-------------------------------- MODULE Join --------------------------------
(***************************************************************************)
(* The wait of `Runtime::shutdown_timeout` for its threads                *)
(* (src/rt/runtime.rs: `Runtime::shutdown_timeout`, `stop_and_join`,       *)
(* `Runtime::join`; src/rt/workers.rs: `Exit`, `take_monitor`,             *)
(* `take_threads`).                                                        *)
(*                                                                          *)
(* Who acts. The joiner J, which waits for the monitor's thread M first,   *)
(* then for the workers W0 and W1, up to one deadline. Each of those       *)
(* threads ends at some moment: it sends its exit message (the drop of its *)
(* `Exit`, once what it held is dropped), and only then reads as finished. *)
(* A thread X outside both lists (a detached one, or one that exited      *)
(* earlier) sends a message too. The deadline passes at any moment, or     *)
(* never.                                                                  *)
(*                                                                          *)
(* What can happen. Each message names its thread. The joiner's wait of a *)
(* list keeps going while one thread of it has neither read as finished    *)
(* nor sent its message, each received message only waking it; what it     *)
(* received while it waited for M counts for the workers' wait too. It    *)
(* stops at the deadline.                                                  *)
(*                                                                          *)
(* What must never happen. The wait ends, the deadline not passed, while   *)
(* a thread of its list still runs (then `shutdown_timeout` returns while  *)
(* a worker drops its shard's tasks, and the drop's lingers start early).  *)
(* And with every thread of the list ended, the wait goes on to the        *)
(* deadline, or for good with none.                                        *)
(*                                                                          *)
(* What is abstract. The channel is a sequence of names; a receive takes   *)
(* its head, or waits. Joining the finished threads after the wait, and    *)
(* leaving the rest to end on their own, is not modelled.                  *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC                                 \* numbers, sequences, set sizes, TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Mutation    \* "none", or the name of the one rule this run takes away

Workers == <<"W0", "W1">>                                                    \* the workers' list, in its order
Threads == {"M", "W0", "W1", "X"}                                            \* every thread that sends

(* --algorithm Join                                                          \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    msgs = <<>>,                     \* the exit channel: the names sent and not received
    sent = [t \in Threads |-> FALSE],    \* each thread sent its exit
    finished = [t \in Threads |-> FALSE], \* each thread reads as finished
    deadline = FALSE,                \* the deadline passed
    exited = {},                     \* the names the joiner received
    phase = "M",                     \* the list the joiner waits for: "M", then "W", then "over"
    i = 1;                           \* (mutated, today's loop) the next worker of the list to wait one message for

define                                                                       \* names the steps and properties use:
    \* The threads of the list the joiner waits for now.
    Listed == IF phase = "M" THEN {"M"} ELSE {"W0", "W1"}                    \* the monitor first
    \* One of them still to wait for: not finished and its message not received.
    \* MUTATION finished_only: a received message counts for nothing: only reading as finished does.
    Waiting == {t \in Listed : ~finished[t] /\ (t \notin exited \/ Mutation = "finished_only")}  \* by its own end
    \* At the end of the workers' wait, unless the deadline passed, each of them has ended.
    WaitsForAll == phase = "over" => deadline \/ \A t \in {"W0", "W1"} : sent[t]  \* never early
    \* The wait ends.
    Returns == <>(phase = "over")                                            \* in the end
    \* Witness: the joiner holds a thread's message while that thread does not read as finished yet.
    NoMessageFirst == \A t \in exited : finished[t]                          \* claims to: never
end define;                                                                  \* end of the names

\* A thread of the runtime: it sends its exit, then reads as finished.
fair process thread \in Threads                                              \* weakly fair: each ends
begin                                                                        \* a thread's steps:
T_exit:                                                                      \* `Exit::drop`
    msgs := Append(msgs, self);                                              \* its message, named
    sent[self] := TRUE;                                                      \* (record)
T_finish:                                                                    \* the thread's main returned
    finished[self] := TRUE;                                                  \* `is_finished` reads true
end process;                                                                 \* end of a thread

\* The deadline: it passes at some moment, or never (no fairness).
process clock = "C"                                                          \* not fair: it may never pass
begin                                                                        \* its one step:
C_pass:                                                                      \* the deadline passes
    deadline := TRUE;                                                        \* from now on, `recv_deadline` times out
end process;                                                                 \* end of the clock

\* The joiner: the monitor's wait, then the workers'.
fair process joiner = "J"                                                    \* weakly fair: it goes on
begin                                                                        \* the joiner's steps:
J_look:                                                                      \* the wait's look at its list
    if phase = "over" then goto Done;                                        \* both lists waited for
    \* MUTATION count_messages: one receive per worker not finished as the loop reaches it, as before the fix.
    elsif Mutation = "count_messages" /\ phase = "W" then                    \* (mutated) today's loop:
        if i > Len(Workers) then phase := "over"; goto J_look;               \* every worker passed: it ends
        elsif finished[Workers[i]] then i := i + 1; goto J_look;             \* finished: the next one
        else goto J_recv;                                                    \* a receive for this one
        end if;                                                              \* one step of it
    elsif Waiting = {} then                                                  \* none of the list left to wait for:
        phase := IF phase = "M" THEN "W" ELSE "over";                        \* the next list, or the end
        \* MUTATION forget_exits: what the monitor's wait received is forgotten for the workers'.
        if Mutation = "forget_exits" then exited := {}; end if;              \* (mutated) a fresh record
        goto J_look;                                                         \* look at it
    else goto J_recv;                                                        \* one to wait for: a receive
    end if;                                                                  \* one of them
J_recv:                                                                      \* `recv_deadline`: blocked until a message or the deadline
    await msgs # <<>> \/ deadline;                                           \* woken
    if msgs # <<>> then                                                      \* a message: it only wakes the wait
        exited := exited \cup {Head(msgs)};                                  \* its name, kept
        msgs := Tail(msgs);                                                  \* taken
        if Mutation = "count_messages" /\ phase = "W" then i := i + 1; end if;  \* (mutated) counted for this worker
    else                                                                     \* the deadline passed:
        phase := IF phase = "M" THEN "W" ELSE "over";                        \* the wait gives up on this list
    end if;                                                                  \* one or the other
    goto J_look;                                                             \* it looks again
end process;                                                                 \* end of the joiner
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

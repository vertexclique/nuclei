-------------------------------- MODULE Job --------------------------------
(***************************************************************************)
(* A blocking job of a Windows handle opened for synchronous IO, and the   *)
(* side holder that cancels it (src/io/source/port.rs, `Job`), at the      *)
(* grain of the atomic steps on the job word and of the system calls.     *)
(*                                                                          *)
(* In plain words: a thread of the pool makes the handle's call (a read    *)
(* that may block for good) for the side, then posts its packet, and then  *)
(* goes on to calls of its own for other files. The side's holder issues   *)
(* the job and may cancel it: `CancelSynchronousIo` on the job's thread    *)
(* ends whatever call that thread is in, so a cancel must never land after *)
(* the job's call, in a later call of the same thread, and must never be   *)
(* lost, or a read with no data to come blocks for good.                  *)
(*                                                                          *)
(* What is modelled step by step: the word's states (idle, queued,         *)
(* aborted, running with the CANCELLING and OUT flags); the job's take of *)
(* the word, its entry into the call, the call's end, its OUT, its free of *)
(* the word; the canceller's load and compare-and-swap, its cancel, its    *)
(* clear of CANCELLING and its retry; the thread's later call of its own.  *)
(* A load followed by a compare-and-swap is one step where a failed swap   *)
(* changes nothing and only retries the load.                              *)
(*                                                                          *)
(* What is abstract: the thread's handle (one thread, so one value); the   *)
(* packet (a count); the call's own result.                               *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Jobs,      \* how many jobs the side issues, one after another
    Blocks,    \* TRUE: a call ends only when cancelled (a read with no data to come)
    Cancel,    \* "always": the holder cancels every job; "maybe": it may or may not
    Mutation   \* "none", or the name of the one rule this run takes away

Idle == [st |-> "idle", c |-> FALSE, o |-> FALSE]
Queued == [st |-> "queued", c |-> FALSE, o |-> FALSE]
Aborted == [st |-> "aborted", c |-> FALSE, o |-> FALSE]
Running == [st |-> "run", c |-> FALSE, o |-> FALSE]

(* --algorithm Job
variables
    word = Idle,        \* the job word
    inCall = "none",    \* the pool thread's call: "none", "job" (the job's), "other" (a later one)
    ended = FALSE,      \* a cancel ended the job's call
    issued = 0,         \* jobs the holder issued
    posted = 0,         \* jobs whose packet was posted
    stray = FALSE,      \* (record) a cancel landed in a call that was not the job's
    aborted = FALSE,    \* (record) a cancel came before the job took the word
    cancelEnded = FALSE,\* (record) a cancel ended the job's call
    retried = FALSE;    \* (record) a canceller tried again

fair process pool = "pool"
variables ran = FALSE;
begin
P_wait:
    await issued > posted;
P_begin:
    \* `begin`: QUEUED to the thread's handle, unless it was cancelled first.
    if word = Queued then
        word := Running;
        ran := TRUE;
    else
        word := Idle;
        ran := FALSE;
    end if;
P_enter:
    if ran then
        inCall := "job";
        ended := FALSE;
    end if;
P_call:
    \* The call ends by itself (unless it blocks for good), or by a cancel.
    if ran then
        if Blocks then
            await ended;
        else
            either skip; or await ended; end either;
        end if;
        inCall := "none";
    end if;
P_out:
    \* `end`: OUT is set with an `or`, which cannot fail.
    \* MUTATION no_out: the job does not say its call is over.
    if ran /\ Mutation # "no_out" then
        word := [word EXCEPT !.o = TRUE];
    end if;
P_free:
    \* ... then the word is freed, once no canceller is at work.
    if ran then
        await ~word.c;
        word := Idle;
    end if;
P_post:
    posted := posted + 1;
P_other:
    \* The thread's next call, of another file: no cancel may land here.
    inCall := "other";
P_other_end:
    inCall := "none";
    if posted < Jobs then goto P_wait; end if;
end process;

fair process holder = "holder"
variables ok = FALSE;
begin
H_issue:
    await word = Idle /\ issued = posted;
    word := Queued;
    issued := issued + 1;
H_choose:
    if Cancel = "maybe" then
        either goto H_load; or goto H_wait; end either;
    end if;
H_load:
    \* `cancel`: look at the word, and take it if it says the job runs.
    if word.st \in {"idle", "aborted"} then
        goto H_wait;
    elsif word.st = "queued" then
        word := Aborted;
        aborted := TRUE;
        goto H_wait;
    elsif (word.c \/ word.o) /\ Mutation # "no_out" then
        goto H_wait;
    elsif Mutation # "no_cancelling" then
        \* MUTATION no_cancelling: the canceller does not hold the job in it.
        word := [word EXCEPT !.c = TRUE];
    end if;
H_cancel:
    \* `CancelSynchronousIo` on the job's thread ends the call it is in, if any.
    if inCall = "job" then
        ended := TRUE;
        cancelEnded := TRUE;
        ok := TRUE;
    elsif inCall = "other" then
        stray := TRUE;
        ok := TRUE;
    else
        ok := FALSE;
    end if;
H_clear:
    \* CANCELLING goes; done if the cancel took or the call is over.
    if Mutation # "no_cancelling" then
        word := [word EXCEPT !.c = FALSE];
    end if;
    \* MUTATION no_retry: the canceller gives up when its cancel finds no call.
    if ok \/ (word.o /\ Mutation # "no_out") \/ Mutation = "no_retry" then
        goto H_wait;
    else
        retried := TRUE;
        goto H_load;
    end if;
H_wait:
    \* The packet of the job: the side issues nothing more before it.
    await posted = issued;
    if issued < Jobs then goto H_issue; end if;
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

\* A cancel never lands in a call that is not the job's.
NoStray == ~stray

\* The word is the job's only while its packet is out.
WordFreeBetweenJobs == (issued = posted /\ pc["pool"] \in {"P_wait", "P_other", "P_other_end"})
                           => word.st \in {"idle"}

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A cancel lands before the job takes the word: the job makes no call.
NoAbort == ~aborted
\* A cancel ends the job's call.
NoCancelEnd == ~cancelEnded
\* A canceller finds the job not in its call yet, and tries again.
NoRetry == ~retried
=============================================================================

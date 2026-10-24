------------------------------- MODULE Task -------------------------------
(***************************************************************************)
(* One task of nuclei's runtime (src/rt/task/), at the grain of the atomic *)
(* steps on its state word.                                                *)
(*                                                                          *)
(* In plain words: a task is a future plus one word of flags and a count   *)
(* of references. A run queue holds "runnables" of it; a runner pops one   *)
(* and polls the future once. The future can hand a waker to another       *)
(* thread, which wakes it later; a wake that lands while the future is     *)
(* being polled only marks the task, and the runner puts it back in the    *)
(* queue after the poll. The task's handle can wait for the output, cancel *)
(* the task, or detach from it. Whoever lets go of the last reference      *)
(* frees the task's memory.                                                *)
(*                                                                          *)
(* What is modelled step by step: every read-modify-write of the state     *)
(* word (each compare-and-swap loop whose new value is a function of the   *)
(* value it read is one step, as a loop that retries until its exchange    *)
(* succeeds behaves exactly like one atomic update), the pushes into the   *)
(* run queue, the awaiter cell and the two-flag handshake that stores and  *)
(* takes it (REGISTERING / NOTIFYING), and the future's drop and output.   *)
(* Where the code does work between its load and its exchange that depends *)
(* on the value it loaded (the run dropping the future of a task it finds  *)
(* closed after its poll, the detach reading the output), the work is its  *)
(* own step, as in the code.                                               *)
(*                                                                          *)
(* What is abstract: the future is one step per poll: Ready once the waker *)
(* thread has set its flag, otherwise Pending, handing its waker to that   *)
(* thread (and, with SelfWake, waking itself once first). The run queue is *)
(* a count of runnables: their order does not matter to one task.          *)
(*                                                                          *)
(* `Mutation` names one rule taken away, so that a configuration can show *)
(* the property that rule protects really is checked (see README).         *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    Runners,   \* the runner threads, numbered 1, 2, ...
    HandleOp,  \* what the handle does: "await", "detach", "drop" or "cancel"
    WakeKind,  \* how the waker thread uses its waker: "by_value", "by_ref" or "drop"
    SelfWake,  \* TRUE: the future wakes itself once during its first poll
    Spurious,  \* how often the handle's executor may poll it again without a wake
    Mutation   \* "none", or the name of the one rule this run takes away

\* The waker thread's and the handle's numbers: no runner uses them.
WakerT == 100
HandleT == 200
\* The larger of two numbers.
Max2(a, b) == IF a > b THEN a ELSE b

(* --algorithm Task
variables
    \* ---- the state word ----
    sch = TRUE,        \* SCHEDULED: a runnable exists (spawn's, in the queue)
    run = FALSE,       \* RUNNING: the future is being polled
    comp = FALSE,      \* COMPLETED: the future returned Ready, its output is stored
    clo = FALSE,       \* CLOSED: cancelled, output taken, or completed with no handle
    tsk = TRUE,        \* TASK: the handle exists
    awt = FALSE,       \* AWAITER: the cell holds the handle's waker
    reg = FALSE,       \* REGISTERING: the handle is storing its waker
    ntf = FALSE,       \* NOTIFYING: someone is taking the stored waker
    refs = 1,          \* the references: the runnable and every waker
    \* ---- the rest of the task and its world ----
    slot = FALSE,      \* the awaiter cell holds the handle's waker
    queue = 1,         \* runnables waiting in the run queue
    future = "live",   \* the future: "live" or "dropped"
    output = "none",   \* "none", "stored", "taken" (by the handle) or "dropped"
    fired = FALSE,     \* the waker thread set what the future waits for
    held = FALSE,      \* the waker thread holds a waker of the task (one reference)
    selfWoken = FALSE, \* the future already woke itself (SelfWake)
    handleWoken = FALSE, \* the handle's own waker was woken: its executor polls it again
    took = [p \in Runners \cup {WakerT, HandleT} |-> FALSE], \* the Take procedure's answer
    \* ---- records the properties read; the code has no such variables ----
    freed = FALSE,     \* the memory was freed
    frees = 0,         \* how many times
    touchedFreed = FALSE, \* some step touched the task after it was freed
    futureDrops = 0,   \* how many times the future was dropped
    futureDropper = "nobody", \* who dropped it: a runner, or someone else
    pollDropped = FALSE, \* a poll ran on a dropped future
    polling = 0,       \* polls in progress right now
    maxPolling = 0,    \* the most polls ever in progress at once
    outputTwice = FALSE, \* the output was read or dropped a second time
    handleGot = "nothing", \* what awaiting the handle returned: "output" or "none"
    wokeRunning = FALSE,   \* the waker thread's wake landed during a poll
    cancelledRunning = FALSE, \* the handle cancelled during a poll
    cancelledQueued = FALSE,  \* a runner found the task cancelled in the queue
    registerMetNotify = FALSE, \* a registration met a notification
    lastWakerClosed = FALSE,   \* the last waker closed an idle task and scheduled it
    parkWoken = FALSE,         \* the handle slept and was woken
    spurious = Spurious;       \* polls without a wake the handle's executor has left

define
    \* Every thread is done.
    AllDone == \A p \in Runners \cup {WakerT, HandleT} : pc[p] = "Done"
end define;

\* Any step that touches the task's memory records it if the memory is gone.
macro touch() begin
    if freed then touchedFreed := TRUE; end if;
end macro;

\* Frees the memory.
macro destroy() begin
    freed := TRUE;
    frees := frees + 1;
end macro;

\* Drops the future; `by` is who did.
macro dropFuture(by) begin
    futureDrops := futureDrops + 1;
    future := "dropped";
    futureDropper := by;
end macro;

\* Reads or drops the output; a second use is recorded.
macro useOutput(to) begin
    if output # "stored" then outputTwice := TRUE; end if;
    output := to;
end macro;

\* `drop_ref`, called by a runner: one reference goes; the last one with no handle frees the
\* task. A task still open then is one nothing can wake or await again (its last waker went
\* during or just after the poll), so its future is dropped first, here on the runner.
macro dropRef() begin
    touch();
    \* MUTATION free_ignores_handle: the last reference frees the task though the handle lives.
    if refs = 1 /\ (~tsk \/ Mutation = "free_ignores_handle") then
        \* MUTATION pending_leaks_future: the future of a task still open is left behind
        \* (what the established task crate does).
        if ~(comp \/ clo) /\ Mutation # "pending_leaks_future" then dropFuture("runner"); end if;
        refs := 0;
        destroy();
    else
        refs := refs - 1;
    end if;
end macro;

\* `drop_waker`: as dropRef, but the last waker of a task that is still open (nobody can
\* wake it, nobody awaits it) closes it and schedules it once more, keeping one reference
\* for that runnable, so its future is dropped by a runner.
macro dropWaker() begin
    touch();
    if refs = 1 /\ ~tsk then
        if ~(comp \/ clo) then
            sch := TRUE;                    \* the word becomes SCHEDULED | CLOSED | one reference
            clo := TRUE;
            awt := FALSE;
            queue := queue + 1;             \* and the runnable goes to the queue
            lastWakerClosed := TRUE;        \* (record)
        else
            refs := 0;
            destroy();
        end if;
    else
        refs := refs - 1;
    end if;
end macro;

(***************************************************************************)
(* `Header::take`: takes the stored waker out. `own` is TRUE when the     *)
(* taker is the handle itself (`notify(Some(cx.waker()))`): its own waker  *)
(* is dropped, not woken. Leaves the answer in took[self]: TRUE when a     *)
(* waker was taken that the caller must wake.                              *)
(***************************************************************************)
procedure Take(own)
begin
T_or:
    \* fetch_or(NOTIFYING): a notification or a registration already in progress keeps the cell.
    touch();
    took[self] := FALSE;
    if ntf \/ reg then
        ntf := TRUE;
        return;
    else
        ntf := TRUE;
    end if;
T_cell:
    \* The cell is ours: take the waker.
    touch();
    took[self] := slot /\ ~own;
    slot := FALSE;
T_and:
    \* fetch_and(!NOTIFYING & !AWAITER).
    touch();
    ntf := FALSE;
    awt := FALSE;
    return;
end procedure;

(***************************************************************************)
(* `Header::register`, called by the handle only: stores its waker.       *)
(***************************************************************************)
procedure Register()
begin
G_try:
    \* A notification in progress would miss the waker: wake it at once instead.
    touch();
    if ntf then
        handleWoken := TRUE;
        registerMetNotify := TRUE;         \* (record)
        return;
    else
        reg := TRUE;
    end if;
G_store:
    \* REGISTERING is ours: store the waker.
    touch();
    slot := TRUE;
G_end:
    \* Clear REGISTERING; a notification that came meanwhile took nothing, so take the waker
    \* back and wake it here.
    touch();
    if ntf then
        registerMetNotify := TRUE;         \* (record)
        if slot then handleWoken := TRUE; end if;
        awt := ~slot;
        slot := FALSE;
    else
        awt := TRUE;
    end if;
    ntf := FALSE;
    reg := FALSE;
    return;
end procedure;

(***************************************************************************)
(* A runner pops a runnable and runs it (`RawTask::run`).                 *)
(***************************************************************************)
fair process runner \in Runners
variables ready = FALSE, prevAwt = FALSE, prevTsk = FALSE, prevClo = FALSE,
          dropped = FALSE;
begin
R_pop:
    \* Wait for a runnable; once the other threads are done and the queue is empty, stop.
    await queue > 0 \/ (pc[WakerT] = "Done" /\ pc[HandleT] = "Done");
    if queue = 0 then
        goto Done;
    else
        queue := queue - 1;
        dropped := FALSE;
    end if;
R_start:
    \* The first exchange of the run: a task cancelled in the queue is not polled.
    touch();
    if clo then
        goto R_closed;
    else
        sch := FALSE;
        run := TRUE;
        polling := polling + 1;
        maxPolling := Max2(maxPolling, polling);
    end if;
R_poll:
    \* The future's poll.
    touch();
    if future = "dropped" then pollDropped := TRUE; end if;
    if fired then
        ready := TRUE;
    else
        ready := FALSE;
        if SelfWake /\ ~selfWoken then
            \* The future wakes itself (`wake_by_ref` on its own waker, which is running).
            selfWoken := TRUE;
            if ~(comp \/ clo) then sch := TRUE; end if;
        end if;
        \* Pending: it hands a waker (a new reference) to the waker thread, if that holds none.
        if ~held then
            refs := refs + 1;
            held := TRUE;
        end if;
    end if;
    polling := polling - 1;
R_branch:
    if ready then goto R_store; else goto R_pend_drop; end if;
R_store:
    \* Ready: the future is dropped and its output written where it was.
    touch();
    dropFuture("runner");
    output := "stored";
R_complete:
    \* Not running, not scheduled, completed; closed too when no handle is left to take it.
    touch();
    prevAwt := awt;
    prevTsk := tsk;
    prevClo := clo;
    run := FALSE;
    sch := FALSE;
    comp := TRUE;
    if ~tsk then clo := TRUE; end if;
R_out:
    \* With no handle, or a handle that cancelled during the poll, the run drops the output.
    \* MUTATION complete_leaks_output: it leaves the output behind.
    if (~prevTsk \/ prevClo) /\ Mutation # "complete_leaks_output" then
        useOutput("dropped");
    end if;
    goto R_finish;
R_pend_drop:
    \* Pending, and found closed: the canceller left the future to this run.
    touch();
    if clo /\ ~dropped then
        dropFuture("runner");
        dropped := TRUE;
    end if;
R_pending:
    \* The exchange after a Pending poll.
    touch();
    if clo /\ ~dropped then
        goto R_pend_drop;                    \* closed just now: drop the future first
    elsif clo then
        prevAwt := awt;
        run := FALSE;
        sch := FALSE;
        goto R_finish;
    elsif sch then
        run := FALSE;
        \* MUTATION run_drops_woken: a task woken during its poll is not scheduled again.
        if Mutation = "run_drops_woken" then goto R_drop; else goto R_resched; end if;
    else
        run := FALSE;
        goto R_drop;
    end if;
R_resched:
    \* Woken during the poll: the run's reference goes to the new runnable.
    touch();
    queue := queue + 1;
    goto R_pop;
R_drop:
    dropRef();
    goto R_pop;
R_closed:
    \* Cancelled while it waited in the queue: drop the future, then clear SCHEDULED.
    touch();
    dropFuture("runner");
    cancelledQueued := TRUE;                 \* (record)
R_closed_clear:
    touch();
    prevAwt := awt;
    sch := FALSE;
R_finish:
    \* `finish`: take the awaiter, drop the run's reference, then wake the awaiter.
    if prevAwt then call Take(FALSE); end if;
R_finish_ref:
    dropRef();
R_finish_wake:
    if took[self] then handleWoken := TRUE; took[self] := FALSE; end if;
    goto R_pop;
end process;

(***************************************************************************)
(* The thread the future handed its waker to: it sets the flag the future *)
(* waits for, then wakes the task (by value, by reference then a drop), or *)
(* only drops every waker it is handed (WakeKind "drop").                  *)
(***************************************************************************)
fair process wakerThread = WakerT
begin
W_fire:
    if WakeKind # "drop" then fired := TRUE; end if;
W_take:
    \* No waker yet: the future's next poll sees the flag. (With "drop", wait for one.)
    if WakeKind = "drop" then
        goto W_wait;
    elsif ~held then
        goto Done;
    else
        goto W_wake;
    end if;
W_wait:
    \* ("drop") Wait for a waker, or for the future to be gone without one.
    await held \/ future = "dropped" \/ freed;
    if held then goto W_drop; else goto Done; end if;
W_wake:
    \* The wake's exchange.
    touch();
    if comp \/ clo then
        goto W_drop;
    elsif sch \/ run then
        if run /\ ~sch then wokeRunning := TRUE; end if;   \* (record)
        \* MUTATION wake_running_schedules: a wake during the poll schedules a second runnable.
        if run /\ ~sch /\ Mutation = "wake_running_schedules" then
            sch := TRUE;
            if WakeKind = "by_ref" then refs := refs + 1; end if;
            goto W_push;
        else
            sch := TRUE;                     \* only marked: the run reschedules it
            goto W_drop;
        end if;
    else
        sch := TRUE;
        \* By reference, the new runnable gets a reference of its own.
        \* MUTATION wake_no_ref: it does not.
        if WakeKind = "by_ref" /\ Mutation # "wake_no_ref" then refs := refs + 1; end if;
        goto W_push;
    end if;
W_push:
    \* The schedule pushes the runnable (by value, the waker's reference goes with it).
    touch();
    queue := queue + 1;
    if WakeKind = "by_value" then
        held := FALSE;
        goto Done;
    end if;
W_drop:
    dropWaker();
    held := FALSE;
    \* ("drop") It drops every waker the future hands it, until the future is gone.
    if WakeKind = "drop" then goto W_wait; end if;
end process;

(***************************************************************************)
(* The handle: HandleOp is "await" (poll it until it returns, then drop  *)
(* it), "detach", "drop" (cancel, then detach) or "cancel" (cancel, await *)
(* the end, then drop).                                                    *)
(***************************************************************************)
fair process handle = HandleT
variables cPrevAwt = FALSE, cPush = FALSE, tPrevAwt = FALSE, dPush = FALSE, dRead = FALSE;
begin
H_begin:
    if HandleOp = "detach" then goto H_detach;
    elsif HandleOp = "await" then goto H_poll;
    end if;
H_cancel:
    \* `set_canceled`: close it unless it completed; an idle task is scheduled once more.
    touch();
    if ~(comp \/ clo) then
        cPrevAwt := awt;
        if run then cancelledRunning := TRUE; end if;   \* (record)
        if ~(sch \/ run) then
            clo := TRUE;
            \* MUTATION cancel_drops_future: the cancel drops the idle future itself.
            if Mutation = "cancel_drops_future" then
                dropFuture("handle");
            else
                sch := TRUE;
                refs := refs + 1;
                cPush := TRUE;
            end if;
        else
            clo := TRUE;
        end if;
    end if;
H_cancel_push:
    if cPush then touch(); queue := queue + 1; end if;
H_cancel_notify:
    if cPrevAwt then call Take(FALSE); end if;
H_cancel_wake:
    if took[HandleT] then handleWoken := TRUE; took[HandleT] := FALSE; end if;
    if HandleOp = "cancel" then goto H_poll; else goto H_detach; end if;
H_poll:
    \* `poll_task`.
    touch();
    if clo then
        if sch \/ run then
            call Register();
            goto H_closed_recheck;
        else
            goto H_closed_notify;
        end if;
    elsif ~comp then
        call Register();
        goto H_recheck;
    else
        goto H_take;
    end if;
H_closed_recheck:
    \* Closed: wait until the future is gone.
    touch();
    if sch \/ run then goto H_park; end if;
H_closed_notify:
    call Take(TRUE);
H_none:
    handleGot := "none";
    goto H_detach;
H_recheck:
    \* The state may have changed just before the registration.
    \* MUTATION no_recheck: the handle sleeps without looking again.
    if Mutation = "no_recheck" then goto H_park; end if;
H_recheck_look:
    touch();
    if clo then goto H_poll;
    elsif ~comp then goto H_park;
    end if;
H_take:
    \* Completed: close it to take the output.
    touch();
    tPrevAwt := awt;
    clo := TRUE;
H_take_notify:
    if tPrevAwt then call Take(TRUE); end if;
H_take_out:
    touch();
    useOutput("taken");
    handleGot := "output";
    goto H_detach;
H_park:
    \* Pending: the handle's executor sleeps until its waker is woken, or polls it again
    \* without a wake, which a future must allow.
    either
        await handleWoken;
        handleWoken := FALSE;
        parkWoken := TRUE;                   \* (record)
    or
        await spurious > 0;
        spurious := spurious - 1;
    end either;
    goto H_poll;
H_detach:
    \* `set_detached`. The common case right after the spawn is one exchange.
    touch();
    if sch /\ tsk /\ refs = 1 /\ ~run /\ ~comp /\ ~clo /\ ~awt /\ ~reg /\ ~ntf then
        tsk := FALSE;
        goto Done;
    end if;
H_det_loop:
    touch();
    if comp /\ ~clo then
        \* Completed and not taken: close it to take the output.
        clo := TRUE;
        dRead := TRUE;
        goto H_det_read;
    elsif refs = 0 /\ ~clo then
        \* The last reference with the task open: close it and schedule it once more.
        sch := TRUE;
        clo := TRUE;
        awt := FALSE;
        refs := 1;
        tsk := FALSE;
        dPush := TRUE;
        goto H_det_push;
    elsif refs = 0 /\ Mutation # "detach_skips_last" then
        \* The last reference of a closed task: free it.
        \* MUTATION detach_skips_last: the handle goes without freeing it.
        tsk := FALSE;
        destroy();
        goto H_det_push;
    else
        tsk := FALSE;
        goto H_det_push;
    end if;
H_det_read:
    \* The output is read out, to be dropped once the handle is gone.
    touch();
    useOutput("dropped");
    goto H_det_loop;
H_det_push:
    if dPush then touch(); queue := queue + 1; end if;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties. Each says one thing the task promises.                  *)
(***************************************************************************)

\* Never two polls of the future at once.
OnePoll == maxPolling <= 1

\* The future is dropped at most once, and only by a runner (so a task bound to one thread
\* has its future dropped on that thread).
FutureOnce == futureDrops <= 1 /\ futureDropper \in {"nobody", "runner"}

\* No poll ever runs on a dropped future.
NoPollAfterDrop == ~pollDropped

\* Nothing touches the task after its memory is freed, and it is freed at most once.
NothingAfterFree == ~touchedFreed /\ frees <= 1 /\ refs >= 0

\* The output is read or dropped at most once.
OutputOnce == ~outputTwice

\* When the memory goes, the future and the output are already gone.
FreedAfterFuture == freed => (futureDrops = 1 /\ output # "stored")

\* Once every thread is done: the memory is freed, the future was dropped, and no output is
\* left behind.
CleanEnd == AllDone => (freed /\ futureDrops = 1 /\ output # "stored")

\* A handle that awaits a task nobody cancels gets its output.
AwaitGetsOutput == (HandleOp = "await" /\ pc[HandleT] = "Done") => handleGot = "output"

\* All the safety properties together.
Safe == OnePoll /\ FutureOnce /\ NoPollAfterDrop /\ NothingAfterFree /\ OutputOnce
        /\ FreedAfterFuture /\ CleanEnd /\ AwaitGetsOutput

(***************************************************************************)
(* Witnesses. Each one claims an interesting case never happens; a         *)
(* configuration checks it and expects it to break, which proves the       *)
(* search really reaches that case.                                        *)
(***************************************************************************)

\* The waker thread's wake lands while the future is being polled.
NoWakeWhileRunning == ~wokeRunning
\* The handle cancels while the future is being polled.
NoCancelWhileRunning == ~cancelledRunning
\* A runner finds the task cancelled while its runnable waited in the queue.
NoCancelInQueue == ~cancelledQueued
\* A registration of the handle's waker meets a notification in progress.
NoRegisterMeetsNotify == ~registerMetNotify
\* The last waker of a detached, idle task closes it and schedules it.
NoLastWakerClose == ~lastWakerClosed
\* The handle sleeps and is woken by the completion.
NoParkWoken == ~parkWoken
=============================================================================

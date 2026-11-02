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
(* The task's own code may panic outside the poll's catch (`Panics`): the *)
(* future's drop (and the terminate hook after it), the drop of an output  *)
(* nobody takes, the drop of what the memory holds as it is freed, a hook  *)
(* around the poll, the runtime's shutdown on a task's panic. Each panic   *)
(* is caught where it happens. A panic of the poll or of the future's drop *)
(* becomes the task's outcome for its handle, even a handle that           *)
(* cancelled: a cancel whose future's drop panicked ends as a completion   *)
(* with that panic. With no handle left, a panic is logged. What must      *)
(* never happen: a panic that aborts, a panic nobody is told of, a handle  *)
(* that awaits and misses the panic.                                       *)
(*                                                                          *)
(* The handle's waker is user code too ("wake", "awdrop", "clone"): each   *)
(* wake or drop of it runs once the change of the word that called for it *)
(* is complete, in a catch that logs a panic; it is cloned before a        *)
(* registration takes REGISTERING, so a clone that panics ends the poll    *)
(* with the word untouched. A waker run inside a change of the word would  *)
(* leave the word half done when it unwinds; one that aborts takes the     *)
(* process down for its own bug.                                           *)
(*                                                                          *)
(* With `Shutdown`, a runner may drop a runnable instead of running it, as *)
(* the runtime's shutdown does (`Runnable::drop`); with `PushStopped`, the *)
(* waker thread's push may find the shard stopped and drop the runnable it *)
(* pushed itself, on its own thread (`Shared::push_remote`). Either drop   *)
(* closes the task, drops the future, clears SCHEDULED, then wakes the     *)
(* awaiter, in that order (the `Unrun` procedure).                          *)
(*                                                                          *)
(* The push from another thread (`Sched::schedule_elsewhere`) holds a      *)
(* waker of its own (`keep`) across the push: once the runnable is in the  *)
(* inbox, the shard may run the task to its end and free it while the push *)
(* still reads what the task's schedule keeps alive (the shard's `Shared`, *)
(* the home and its target).                                               *)
(*                                                                          *)
(* The handle may poll once and let go while the task is pending: give_up *)
(* drops it, a cancel with its waker stored; poll_detach detaches it,      *)
(* its waker left in the cell for the completion or the free). Its         *)
(* executor may poll it with a new waker after each sleep (HandleWakers),  *)
(* so a registration that meets a notification looks at which waker is     *)
(* stored (`stored_wakes_as`). An abort handle (`Aborter`, `AbortHandle`)  *)
(* is a waker clone, so one reference, on a thread of its own: it cancels  *)
(* the task, then is dropped. The run's completion decides its claim of   *)
(* the awaiter on the word it read before the future's drop, which a      *)
(* cancel's notify may have made stale.                                    *)
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
    Panics,    \* the task's own code that panics: a subset of "future" (its drop), "output"
               \* (an output's drop), "free" (the drop as the memory goes), "hook" (around the
               \* first poll) and "shutdown" (the shutdown on a task's panic); and the handle's
               \* waker, user code too: "wake" (each wake of it panics, after the wake took
               \* effect), "awdrop" (each drop of it panics) and "clone" (one clone of it panics,
               \* at a registration while a waker of an earlier one is stored)
    Shutdown,  \* TRUE: a runner may drop a runnable instead of running it
    PushStopped, \* TRUE: the waker thread's push may find the shard stopped and drop the runnable itself
    HandleWakers, \* 1, or 2: the handle's executor may poll it with a new waker each time
    Aborter,   \* TRUE: an abort handle, made right after the spawn, cancels the task from a thread of its own
    Mutation   \* "none", or the name of the one rule this run takes away

\* The waker thread's, the handle's and the abort handle's numbers: no runner uses them.
WakerT == 100
HandleT == 200
AbortT == 300
\* The abort handle's thread, when this run has one.
Abort == IF Aborter THEN {AbortT} ELSE {}
\* Every thread of the run.
Threads == Runners \cup {WakerT, HandleT} \cup Abort
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
    refs = IF Aborter /\ Mutation # "abort_no_ref" THEN 2 ELSE 1,
                       \* the references: the runnable, every waker, and the abort handle's
                       \* (a waker clone, `Task::abort_handle`). MUTATION abort_no_ref: the abort
                       \* handle holds none.
    \* ---- the rest of the task and its world ----
    slot = 0,          \* the awaiter cell: 0 when empty, else the number of the handle's waker it holds
    stored = 0,        \* the waker last stored there (`awaiter_data`, `awaiter_table`), 0 for none yet
    queue = 1,         \* runnables waiting in the run queue
    future = "live",   \* the future: "live" or "dropped"
    output = "none",   \* "none", "stored", "taken" (by the handle), "out" (taken out by the
                       \* run, to be dropped once its exchange is done) or "dropped"
    fired = FALSE,     \* the waker thread set what the future waits for
    held = FALSE,      \* the waker thread holds a waker of the task (one reference)
    selfWoken = FALSE, \* the future already woke itself (SelfWake)
    woken = {},        \* the handle's wakers woken: its executor polls it again once its own is
    cur = 1,           \* the waker the handle's executor polls it with now (1, or 2 after a change)
    took = [p \in Threads |-> 0], \* the waker a Take or a claim took to wake, 0 for none
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
    freer = "nobody",          \* who freed the memory: "runner", "waker" or "handle"
    awaitedAtComplete = FALSE, \* the handle still held the task as it completed
    spurious = Spurious,       \* polls without a wake the handle's executor has left
    \* ---- where each panic of the task's own code went: "none" (it did not panic), "raised"
    \* (caught, held by the thread that caught it), "outcome" (it is the task's stored outcome),
    \* "told" (resumed in the handle, or logged), "silent" (dropped untold) or "aborted" ----
    hookFate = "none",         \* a hook around the poll
    dropFate = "none",         \* the future's drop
    valueFate = "none",        \* the drop of an output (a value) the runtime lets go of
    freeFate = "none",         \* the drop of what the memory holds, as it is freed
    shutFate = "none",         \* the runtime's shutdown on the task's panic
    unclosed = FALSE,          \* (record) a cancel ended as a completion with a panic, for the handle
    shutDropped = FALSE,       \* (record) a runnable was dropped without being run
    aborted = FALSE,           \* (record) the abort handle closed the task
    cellRace = FALSE,          \* (record) a take reached the awaiter cell while a registration held it
    staleMet = FALSE,          \* (record) a registration met a notification with an older waker stored
    cancelNotified = FALSE,    \* (record) the handle's cancel took a waker it had stored
    claimedEmpty = FALSE,      \* (record) the completion's claim found the cell empty
    \* ---- the handle's waker that panics ----
    wakerFate = "none",        \* where its panics went (as the fates above): "told" once caught and logged,
                               \* or handed to the handle's poller; "aborted" as it once did
    regPanicked = FALSE,       \* the handle's registration ended with a panic: its poll unwinds
    clonePanicked = FALSE,     \* (record) the one clone that panics did
    unwound = [p \in Threads |-> FALSE]; \* a take whose wake panicked inside it unwound its caller

define
    \* Every thread is done.
    AllDone == \A p \in Threads : pc[p] = "Done"
    \* The stored outcome is a panic (of a hook or of the future's drop), not the future's value.
    OutcomePanic == hookFate = "outcome" \/ dropFate = "outcome"
    \* The output the run takes out of the task and drops itself: any, with no handle left; a
    \* value of a handle that cancelled. A panic stays for a handle, even one that cancelled.
    \* MUTATION cancel_panic_logged: a cancelled task's panic is dropped (and logged) too.
    Unclaimed == ~tsk \/ (clo /\ (~OutcomePanic \/ Mutation = "cancel_panic_logged"))
end define;

\* Any step that touches the task's memory records it if the memory is gone.
macro touch() begin
    if freed then touchedFreed := TRUE; end if;
end macro;

\* Wakes the handle's waker `w` (`Waker::wake`), once the change of the word that called for the
\* wake is complete: the wake may panic ("wake"), after it took effect, and the panic is caught
\* and logged (`caught::catch`, `lost`). MUTATION waker_aborts: the panic aborts the process, as
\* `abort_on_panic` once made it.
macro wakeAwaiter(w) begin
    woken := woken \cup {w};
    if "wake" \in Panics then
        wakerFate := IF Mutation = "waker_aborts" THEN "aborted" ELSE "told";
    end if;
end macro;

\* Drops a waker of the handle's, the change of the word complete: the drop may panic
\* ("awdrop"), caught and logged. MUTATION waker_aborts: the panic aborts, as it once did.
macro dropAwaiter() begin
    if "awdrop" \in Panics then
        wakerFate := IF Mutation = "waker_aborts" THEN "aborted" ELSE "told";
    end if;
end macro;

\* Frees the memory; `by` is who did. The drop of what it holds (the schedule, a stored waker)
\* may panic ("free"): caught and logged, and the memory freed all the same.
\* MUTATION free_skips: a free whose drop panicked skips the free itself.
\* MUTATION free_aborts: the drop's panic aborts the process, as it once did.
macro destroy(by) begin
    if ~("free" \in Panics /\ Mutation = "free_skips") then
        freed := TRUE;                       \* the memory goes back
        frees := frees + 1;                  \* (record) once more
        \* A waker still in the awaiter cell goes with what the memory holds (`drop_in_place`).
        \* MUTATION destroy_keeps_awaiter: the free leaves it there, never dropped (a leak of
        \* the awaiter's own task).
        if Mutation # "destroy_keeps_awaiter" then slot := 0; end if;
        freer := by;                         \* (record) by whom
    end if;
    if "free" \in Panics then
        freeFate := IF Mutation = "free_aborts" THEN "aborted" ELSE "told";
    end if;
end macro;

\* The future's drop (with the terminate hook after it) panics, if this run says so ("future"):
\* the dropper catches the panic and holds it until the task is settled.
\* MUTATION drop_aborts: the panic aborts the process, as it once did.
macro futurePanics() begin
    if "future" \in Panics then
        dropFate := IF Mutation = "drop_aborts" THEN "aborted" ELSE "raised";
    end if;
end macro;

\* The exchange that ends a cancel after the future's drop (`settle`): with the drop's panic
\* held and the handle there, the task completes with that panic for the handle, closed no
\* more (`settle_panicked`); with no handle, the panic is logged. Without a panic it only
\* clears the run's flags, which the step that uses it does.
\* MUTATION cancel_panic_logged: the panic is logged though the handle is there.
\* MUTATION unclose_ignores_handle: the task completes with the panic though no handle is left.
macro unclose() begin
    if dropFate = "raised" then
        if (tsk /\ Mutation # "cancel_panic_logged") \/ Mutation = "unclose_ignores_handle" then
            clo := FALSE;                    \* closed no more
            comp := TRUE;                    \* completed, its outcome stored
            output := "stored";              \* the outcome is there for the handle
            dropFate := "outcome";           \* and it is the drop's panic
            unclosed := TRUE;                \* (record)
        else
            dropFate := "told";              \* nobody to hand it to: logged
        end if;
    end if;
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

\* The reference's drop of `dropRef`, for a step that has touched the task already (the
\* exchange after a Pending poll, which clears RUNNING and drops the reference at once).
macro releaseRef() begin
    \* MUTATION free_ignores_handle: the last reference frees the task though the handle lives.
    if refs = 1 /\ (~tsk \/ Mutation = "free_ignores_handle") then
        \* MUTATION pending_leaks_future: the future of a task still open is left behind
        \* (what the established task crate does).
        if ~(comp \/ clo) /\ Mutation # "pending_leaks_future" then
            dropFuture("runner");
            \* Nobody can await the task: a panic of the drop is logged (`drop_unawaited`).
            if "future" \in Panics then
                dropFate := IF Mutation = "drop_aborts" THEN "aborted" ELSE "told";
            end if;
        end if;
        refs := 0;
        destroy("runner");
    else
        refs := refs - 1;
    end if;
end macro;

\* `drop_ref`, called by a runner: one reference goes; the last one with no handle frees the
\* task. A task still open then is one nothing can wake or await again (its last waker went
\* during or just after the poll), so its future is dropped first, here on the runner.
macro dropRef() begin
    touch();
    releaseRef();
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
            destroy("waker");
        end if;
    else
        refs := refs - 1;
    end if;
end macro;

(***************************************************************************)
(* `Header::take`: takes the stored waker out. `own` is TRUE when the     *)
(* taker is the handle itself (`notify(Some(cx.waker()))`): a stored waker *)
(* that wakes as its current one is dropped, not woken; an older one is    *)
(* woken. Leaves the answer in took[self]: the number of the waker taken   *)
(* that the caller must wake, 0 for none.                                  *)
(***************************************************************************)
procedure Take(own)
variables dropOwn = FALSE;   \* the waker taken is the handle's current one, which it drops
begin
T_or:
    \* fetch_or(NOTIFYING): a notification or a registration already in progress keeps the cell.
    \* MUTATION take_ignores_flags: the take goes on to the cell whatever the flags say.
    touch();
    took[self] := 0;
    if (ntf \/ reg) /\ Mutation # "take_ignores_flags" then
        ntf := TRUE;
        return;
    else
        ntf := TRUE;
    end if;
T_cell:
    \* The cell is ours: take the waker. A registration holding the cell now would be a second
    \* thread in it at once (recorded).
    touch();
    if reg then cellRace := TRUE; end if;
    \* MUTATION wake_in_take: the waker taken is woken here, inside the take, NOTIFYING held; a
    \* wake that panics unwinds out of the take, skipping its fetch_and, and out of its caller,
    \* whose catch skips the rest of the caller's work too.
    if Mutation = "wake_in_take" /\ slot # 0 /\ ~(own /\ slot = cur) then
        woken := woken \cup {slot};
        took[self] := 0;
        slot := 0;
        if "wake" \in Panics then
            wakerFate := "told";             \* caught, out there, and logged
            unwound[self] := TRUE;
            return;
        end if;
    else
        dropOwn := own /\ slot # 0 /\ slot = cur;
        took[self] := IF slot # 0 /\ ~(own /\ slot = cur) THEN slot ELSE 0;
        slot := 0;
    end if;
T_and:
    \* fetch_and(!NOTIFYING & !AWAITER).
    touch();
    ntf := FALSE;
    awt := FALSE;
T_drop:
    \* The handle's own waker taken is dropped, the word's change complete.
    if dropOwn then dropAwaiter(); end if;
    return;
end procedure;

(***************************************************************************)
(* `Header::register`, called by the handle only: stores its waker.       *)
(***************************************************************************)
procedure Register()
variables old = 0;   \* the waker the store replaced, dropped once REGISTERING is let go
begin
G_clone:
    \* The waker is cloned before any flag is taken (`waker.clone()`), so a clone that panics
    \* unwinds out of the poll with the word untouched: the waker stored before stays for the
    \* completion to wake. MUTATION clone_in_register: the clone runs while REGISTERING is held
    \* (in G_store), as it once did.
    if Mutation # "clone_in_register" /\ "clone" \in Panics /\ awt /\ ~clonePanicked then
        clonePanicked := TRUE;               \* (record)
        regPanicked := TRUE;                 \* the handle's poll unwinds
        wakerFate := "told";                 \* to the handle's poller, which caught it
        return;
    end if;
G_try:
    \* A notification in progress holds the cell. With a waker stored (AWAITER) that wakes as
    \* the one the handle polls with now (`stored_wakes_as`), the notifier took it or takes it,
    \* and wakes it: that wake is this registration's. With none stored, or an older waker
    \* stored (the executor polled the handle with a new one since), the notification would
    \* miss this waker: it is woken at once instead.
    \* MUTATION register_no_self_wake: it is never woken here.
    \* MUTATION register_trusts_stored: any waker stored counts as this one, unlooked at.
    touch();
    if ntf then
        if Mutation # "register_no_self_wake"
           /\ (~awt \/ (stored # cur /\ Mutation # "register_trusts_stored")) then
            wakeAwaiter(cur);               \* `wake_by_ref`: the handle polls again
        end if;
        registerMetNotify := TRUE;         \* (record)
        if awt /\ stored # cur then staleMet := TRUE; end if;   \* (record)
        return;
    else
        reg := TRUE;
    end if;
G_store:
    \* REGISTERING is ours: store the waker (the clone made above; one stored before is
    \* dropped below, once the flag is let go), and name it in `awaiter_data` and
    \* `awaiter_table`.
    \* MUTATION clone_in_register: the clone, and the drop of the waker it replaces, run here
    \* with REGISTERING held: one that panics unwinds out of the poll with the flag left set.
    touch();
    if Mutation = "clone_in_register" /\ awt /\ ~clonePanicked
       /\ ("clone" \in Panics \/ ("awdrop" \in Panics /\ slot # 0)) then
        clonePanicked := TRUE;               \* (record)
        regPanicked := TRUE;                 \* the handle's poll unwinds
        wakerFate := "told";                 \* to the handle's poller, which caught it
        return;
    end if;
G_put:
    \* The store itself.
    touch();
    old := slot;
    slot := cur;
    stored := cur;
G_end:
    \* Clear REGISTERING; a notification that came meanwhile took nothing, so take the waker
    \* back and wake it here.
    \* MUTATION register_end_keeps: the waker stays stored, the notification that came missed.
    touch();
    if ntf /\ Mutation # "register_end_keeps" then
        registerMetNotify := TRUE;         \* (record)
        if slot # 0 then wakeAwaiter(slot); end if;
        awt := slot = 0;
        slot := 0;
    else
        awt := TRUE;
    end if;
    ntf := FALSE;
    reg := FALSE;
G_drop_old:
    \* The waker the store replaced is dropped, REGISTERING let go.
    if old # 0 then dropAwaiter(); end if;
    return;
end procedure;

(***************************************************************************)
(* `Runnable::drop` (`RawTask::drop_runnable`): a runnable dropped without *)
(* being run, by whoever holds it: a runner at the runtime's shutdown (a   *)
(* shard's queues as it goes, `release_inbox`), or the waker thread whose  *)
(* push found the shard stopped (`Shared::push_remote`, `drop_stopped`).   *)
(* `by` names the dropper. It closes the task unless it completed or is    *)
(* closed already, drops the future it owns, clears SCHEDULED in one       *)
(* exchange that settles the drop's panic, takes a stored waker and wakes  *)
(* it, and lets go of its reference. The order is what a handle that       *)
(* cancelled relies on: it sees the task closed, waits while SCHEDULED or  *)
(* RUNNING is set, and is woken once the future is gone.                   *)
(***************************************************************************)
procedure Unrun(by)
variables uAwt = FALSE;   \* the word the clearing exchange read showed a stored waker
begin
U_close:
    \* Close it unless it completed or is closed already.
    \* MUTATION unrun_no_close: it stays open, the future then dropped under an open task.
    touch();
    if ~(comp \/ clo) /\ Mutation # "unrun_no_close" then clo := TRUE; end if;
    shutDropped := TRUE;                     \* (record)
U_early:
    \* MUTATION unrun_clear_first: SCHEDULED is cleared (and the word read) before the future's
    \* drop, so a handle that cancelled sees the task closed and its future gone while it lives.
    if Mutation = "unrun_clear_first" then
        touch();
        sch := FALSE;
        uAwt := awt;
    end if;
U_future:
    \* The runnable owns the future: it drops it (the drop may panic: caught, held).
    touch();
    dropFuture(by);
    futurePanics();
U_clear:
    \* One exchange clears SCHEDULED, settling the drop's panic with it (`settle`).
    touch();
    if Mutation # "unrun_clear_first" then
        sch := FALSE;
        uAwt := awt;
    end if;
    unclose();
U_notify:
    \* A stored waker is taken (`notify(None)`) ...
    \* MUTATION unrun_no_notify: it is left there, and the awaiter is never woken.
    if uAwt /\ Mutation # "unrun_no_notify" then call Take(FALSE); end if;
U_wake:
    \* ... and woken. (wake_in_take: a wake that panicked inside the take unwound the drop too,
    \* its reference never let go of.)
    if unwound[self] then
        unwound[self] := FALSE;
        return;
    end if;
U_waken:
    if took[self] # 0 then wakeAwaiter(took[self]); took[self] := 0; end if;
U_ref:
    \* The runnable's reference goes.
    dropRef();
    return;
end procedure;

(***************************************************************************)
(* A runner pops a runnable and runs it (`RawTask::run`).                 *)
(***************************************************************************)
fair process runner \in Runners
variables ready = FALSE, prevAwt = FALSE, prevTsk = FALSE, prevClo = FALSE,
          dropped = FALSE, claimed = FALSE, claimNow = FALSE, outDropped = FALSE,
          seenAwt = FALSE;   \* the word the completion decides its claim on showed a stored waker
begin
R_pop:
    \* Wait for a runnable; once the other threads are done and the queue is empty, stop.
    await queue > 0 \/ \A p \in {WakerT, HandleT} \cup Abort : pc[p] = "Done";
    if queue = 0 then
        goto Done;
    else
        queue := queue - 1;
        dropped := FALSE;
        \* At the runtime's shutdown a runnable may be dropped instead of run.
        if Shutdown then
            either
                goto R_start;
            or
                goto D_close;
            end either;
        end if;
    end if;
R_start:
    \* The first exchange of the run, one addition: SCHEDULED, which the task of every runnable
    \* has and nothing clears while the runnable waits, becomes RUNNING, whatever else the word
    \* holds. A task cancelled in the queue is then not polled.
    touch();
    sch := FALSE;
    run := TRUE;
    if clo then
        goto R_closed;
    else
        polling := polling + 1;
        maxPolling := Max2(maxPolling, polling);
    end if;
R_poll:
    \* The future's poll.
    touch();
    if future = "dropped" then pollDropped := TRUE; end if;
    if "hook" \in Panics /\ hookFate = "none" then
        \* A hook before or after the first poll panics, inside the poll's catch: the poll's
        \* outcome is that panic. MUTATION hook_aborts: it aborts the process, as it once did.
        hookFate := IF Mutation = "hook_aborts" THEN "aborted" ELSE "raised";
        ready := TRUE;
    elsif fired then
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
    \* Ready: the word is read (`settle_run`), the future is dropped (its drop may panic:
    \* caught, held) and its output written where it was. The claim below decides on that
    \* word, which may be stale by then: a cancel's notify may have taken the waker it showed.
    touch();
    seenAwt := awt;
    dropFuture("runner");
    futurePanics();
    output := "stored";
    outDropped := FALSE;
    claimed := FALSE;
    took[self] := 0;
    \* MUTATION release_after_complete: the run lets go of its reference after the
    \* completion's exchange, as it once did: the exchange, then the awaiter's take, then the
    \* drop. A handle polled in between takes the output and goes, and the run frees the task.
    if Mutation = "release_after_complete" then
        goto R_complete_old;
    elsif hookFate # "raised" /\ "future" \notin Panics then
        goto R_claim;                        \* no panic: the outcome is the value
    end if;
R_outcome:
    \* The outcome is a panic: the poll's own (a hook's) if it had one, the drop's panic then
    \* logged; else the drop's, and the value is dropped, a panic of its drop caught and logged.
    \* The runtime's shutdown on a task's panic may panic too (`Sched::panicked`): caught, logged.
    \* MUTATION complete_keeps_value: the value stays the outcome; the drop's panic is let go of.
    \* MUTATION shutdown_aborts: the shutdown's panic aborts the process, as it once did.
    if hookFate = "raised" then
        hookFate := "outcome";               \* the poll's panic is the outcome
        if dropFate = "raised" then dropFate := "told"; end if;   \* the drop's is logged
    elsif Mutation = "complete_keeps_value" then
        dropFate := "silent";                \* the value stays; the drop's panic goes untold
    else
        dropFate := "outcome";               \* the drop's panic is the outcome
        if "output" \in Panics then valueFate := "told"; end if;   \* the value's drop: logged
    end if;
    if "shutdown" \in Panics /\ Mutation # "complete_keeps_value" then
        shutFate := IF Mutation = "shutdown_aborts" THEN "aborted" ELSE "told";
    end if;
R_claim:
    \* `Header::claim`, when the word the run read shows a stored waker it has not claimed:
    \* fetch_or(NOTIFYING). A registration or a notification in progress keeps the cell and
    \* the flag, and the waker is theirs to wake. On a stale word the cell may be empty now:
    \* the claim holds NOTIFYING over nothing until the exchange, and a registration meanwhile
    \* wakes its own waker (`G_try`).
    if seenAwt /\ ~claimed then
        touch();
        if ~(ntf \/ reg) then
            claimed := TRUE;
            claimNow := TRUE;
        end if;
        ntf := TRUE;
    end if;
R_claim_cell:
    \* The cell is the run's: it takes the waker, NOTIFYING held until the exchange below.
    if claimNow then
        touch();
        if slot = 0 then claimedEmpty := TRUE; end if;   \* (record)
        took[self] := slot;
        slot := 0;
        claimNow := FALSE;
    end if;
R_out:
    \* With no handle, or a handle that cancelled during the poll and an outcome that is a
    \* value, nobody takes the output. A handle that cancelled waits for it to go, so the run
    \* drops it here, before the exchange that clears RUNNING. With no handle, the run takes it
    \* out of the task, while its reference still holds it, and drops it once the task is let
    \* go of (R_out_drop), before the awaiter's wake (a detached blocking job's end so follows
    \* its free). A panic outcome is logged; a value whose drop panics has that panic caught
    \* and logged (`drop_unclaimed`).
    \* MUTATION complete_leaks_output: it leaves the output behind.
    \* MUTATION unclaimed_silent: a panic outcome is dropped unlogged.
    \* MUTATION output_drop_aborts: a value's drop that panics aborts, as it once did.
    \* MUTATION output_after_exchange: the output of a task whose handle cancelled is dropped
    \* after the exchange too, as before the fix: the cancel answers None while it lives.
    touch();
    if Unclaimed /\ ~outDropped /\ Mutation # "complete_leaks_output" then
        useOutput(IF tsk /\ Mutation # "output_after_exchange" THEN "dropped" ELSE "out");
        outDropped := TRUE;
        if hookFate = "outcome" then
            hookFate := IF Mutation = "unclaimed_silent" THEN "silent" ELSE "told";
        elsif dropFate = "outcome" then
            dropFate := IF Mutation = "unclaimed_silent" THEN "silent" ELSE "told";
        elsif "output" \in Panics then
            valueFate := IF Mutation = "output_drop_aborts" THEN "aborted" ELSE "told";
        end if;
    end if;
R_complete:
    \* One exchange: not running, not scheduled, completed (closed too with no handle left),
    \* the flags the claim held cleared, and the run's reference gone. A handle that sees the
    \* completion holds the last reference, so its drop frees the task where it is; with no
    \* handle and no other reference left, the run frees it here.
    touch();
    \* MUTATION complete_no_reclaim: the exchange completes on the new word all the same, and
    \* the waker stored since stays in its cell, never woken.
    if awt /\ ~claimed /\ ~(ntf \/ reg) /\ Mutation # "complete_no_reclaim" then
        \* A waker was stored since, its cell free: the exchange fails on the new word, and
        \* the run claims the waker first.
        seenAwt := awt;
        goto R_claim;
    elsif Unclaimed /\ ~outDropped /\ Mutation # "complete_leaks_output" then
        \* The word changed to one that leaves the output to the run: drop it first.
        goto R_out;
    else
        awaitedAtComplete := tsk;
        run := FALSE;
        sch := FALSE;
        comp := TRUE;
        if ~tsk then
            clo := TRUE;
        elsif clo /\ OutcomePanic then
            \* A panic kept for a handle that cancelled: closed no more, so the handle takes it.
            clo := FALSE;
            unclosed := TRUE;                \* (record)
        end if;
        if claimed then
            ntf := FALSE;
            awt := FALSE;
        end if;
        \* MUTATION free_ignores_handle: the last reference frees the task though the handle
        \* lives.
        if refs = 1 /\ (~tsk \/ Mutation = "free_ignores_handle") then
            refs := 0;
            destroy("runner");
        else
            refs := refs - 1;
        end if;
    end if;
R_out_drop:
    \* The output taken out is dropped, outside the task, once the exchange let go of it: only
    \* a task with no handle left (or, mutated, a cancelled one) took it out.
    if output = "out" then output := "dropped"; end if;
R_complete_wake:
    \* The awaiter is woken; nothing of the task is touched any more.
    if took[self] # 0 then wakeAwaiter(took[self]); took[self] := 0; end if;
    goto R_pop;
R_complete_old:
    \* (release_after_complete) The completion's exchange alone.
    touch();
    prevAwt := awt;
    prevTsk := tsk;
    prevClo := clo;
    awaitedAtComplete := tsk;
    run := FALSE;
    sch := FALSE;
    comp := TRUE;
    if ~tsk then clo := TRUE; end if;
R_out_old:
    \* (release_after_complete) The output dropped after the exchange.
    if ~prevTsk \/ prevClo then
        useOutput("dropped");
    end if;
    goto R_finish;
R_pend_drop:
    \* Pending, and found closed: the canceller left the future to this run (its drop may
    \* panic: caught, held).
    touch();
    if clo /\ ~dropped then
        dropFuture("runner");
        futurePanics();
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
        unclose();                           \* the drop's panic, if any, settled with them
        goto R_finish;
    elsif sch then
        run := FALSE;
        \* MUTATION run_drops_woken: a task woken during its poll is not scheduled again.
        \* MUTATION pending_drops_woken: the exchange drops the run's reference though the
        \* task was woken, and the runnable it schedules holds none.
        if Mutation = "run_drops_woken" then
            goto R_drop;
        elsif Mutation = "pending_drops_woken" then
            releaseRef();
            goto R_resched;
        else
            goto R_resched;
        end if;
    elsif Mutation = "pending_two_steps" then
        \* MUTATION pending_two_steps: RUNNING is cleared first and the reference dropped in a
        \* step of its own, as it once was; a run woken meanwhile on another runner completes
        \* the task, its handle takes the output and goes, and this late drop frees the task.
        run := FALSE;
        goto R_drop;
    else
        \* Neither woken nor closed: RUNNING cleared and the run's reference dropped in one
        \* exchange, so no later run of the task is followed by this one's drop.
        run := FALSE;
        releaseRef();
        goto R_pop;
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
    \* Cancelled while it waited in the queue: drop the future, then clear RUNNING, which the
    \* first exchange set.
    touch();
    dropFuture("runner");
    futurePanics();                          \* the drop may panic: caught, held
    cancelledQueued := TRUE;                 \* (record)
R_closed_clear:
    touch();
    prevAwt := awt;
    \* MUTATION closed_keeps_running: RUNNING stays set, so the task looks polled for ever.
    if Mutation # "closed_keeps_running" then run := FALSE; end if;
    unclose();                               \* the drop's panic, if any, settled with it
R_finish:
    \* `finish`: take the awaiter, drop the run's reference, then wake the awaiter.
    if prevAwt then call Take(FALSE); end if;
R_finish_ref:
    \* (wake_in_take) A wake that panicked inside the take unwound the run's end too.
    if unwound[self] then
        unwound[self] := FALSE;
        goto R_pop;
    end if;
R_finish_drop:
    dropRef();
R_finish_wake:
    if took[self] # 0 then wakeAwaiter(took[self]); took[self] := 0; end if;
    goto R_pop;
D_close:
    \* At the runtime's shutdown the runnable is dropped unrun.
    call Unrun("runner");
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
    \* The schedule pushes the runnable from this thread (`Sched::schedule_elsewhere`; by
    \* value, the waker's reference goes with it). Once pushed, the shard may run the task to
    \* its end and free it, the schedule with it, while the push still reads what the schedule
    \* holds (the shard's `Shared`, the home and its target): first it takes a waker of its own,
    \* `keep`, one reference more.
    \* MUTATION push_no_keep: it takes none, and nothing holds the task across the push.
    touch();
    if Mutation # "push_no_keep" then refs := refs + 1; end if;
    if WakeKind = "by_value" then held := FALSE; end if;
W_queue:
    \* `push_remote` (or `Home::push`): the runnable goes into the inbox.
    touch();
    queue := queue + 1;
W_stopped:
    \* `push_remote` to a shard that stopped drops what the inbox holds, on this thread
    \* (`drop_stopped`): a task of a shard's is `Send`, and may be dropped anywhere. A bound
    \* task's push goes to its home, which drops nothing.
    if PushStopped /\ queue > 0 then
        either
            skip;
        or
            queue := queue - 1;
            call Unrun("waker");
        end either;
    end if;
W_after:
    \* The rest of the push reads what the task's schedule keeps alive: the shard's flags and
    \* its park (`stopped`, `park.wake`), or the home's target.
    touch();
W_unkeep:
    \* `drop(keep)`: the reference it took goes, as a waker's drop does.
    if Mutation # "push_no_keep" then dropWaker(); end if;
    if WakeKind = "by_value" then goto Done; end if;
W_drop:
    dropWaker();
    held := FALSE;
    \* ("drop") It drops every waker the future hands it, until the future is gone.
    if WakeKind = "drop" then goto W_wait; end if;
end process;

(***************************************************************************)
(* The handle: HandleOp is "await" (poll it until it returns, then drop  *)
(* it), "detach", "drop" (cancel, then detach), "cancel" (cancel, await   *)
(* the end, then drop), "give_up" (poll it, and drop it while it is       *)
(* pending: a cancel with its waker stored, then the detach), or          *)
(* "poll_detach" (poll it, then detach it with its waker still stored, as *)
(* a `select!` that lets go of the handle does).                          *)
(***************************************************************************)
fair process handle = HandleT
variables cPrevAwt = FALSE, cPush = FALSE, tPrevAwt = FALSE, dPush = FALSE, dRead = FALSE,
          gaveUp = FALSE;   \* (give_up, poll_detach) it already let go after its first poll
begin
H_begin:
    if HandleOp = "detach" then goto H_detach;
    elsif HandleOp \in {"await", "give_up", "poll_detach"} then goto H_poll;
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
    \* A waker the handle stored before it let go (give_up) is taken (`notify(None)`) and woken.
    if cPrevAwt then
        cancelNotified := TRUE;              \* (record)
        call Take(FALSE);
    end if;
H_cancel_wake:
    if took[HandleT] # 0 then wakeAwaiter(took[HandleT]); took[HandleT] := 0; end if;
    if HandleOp = "cancel" then goto H_poll; else goto H_detach; end if;
H_poll:
    \* `poll_task`.
    touch();
    if clo then
        \* MUTATION closed_no_wait: a closed task answers None at once, its future perhaps
        \* still in a run queue or a poll.
        if (sch \/ run) /\ Mutation # "closed_no_wait" then
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
    \* Closed: wait until the future is gone. A cancel that ended as a completion with a panic
    \* (closed no more) is looked at again, so the handle takes the panic (`poll_raw`).
    \* MUTATION closed_no_recheck: the handle answers "no output" without looking again.
    \* A registration that panicked ends the poll first, as in H_recheck.
    if regPanicked then
        regPanicked := FALSE;
        goto H_sleep;
    end if;
H_closed_look:
    touch();
    if sch \/ run then
        goto H_park;
    elsif ~clo /\ Mutation # "closed_no_recheck" then
        goto H_poll;
    end if;
H_closed_notify:
    call Take(TRUE);
H_none:
    handleGot := "none";
    goto H_detach;
H_recheck:
    \* A registration that panicked ends the poll with the panic: the handle's poller caught
    \* it, and the handle sleeps until its waker is woken, as after a Pending poll.
    if regPanicked then
        regPanicked := FALSE;
        goto H_sleep;
    end if;
H_recheck_on:
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
    \* The output is the handle's: a panic outcome is resumed in the awaiter, so it is told.
    touch();
    useOutput("taken");
    if hookFate = "outcome" then
        hookFate := "told";
        handleGot := "panic";
    elsif dropFate = "outcome" then
        dropFate := "told";
        handleGot := "panic";
    else
        handleGot := "output";
    end if;
    goto H_detach;
H_park:
    \* Pending. A handle that gives up lets go of it now: a drop (cancel, then detach) or a
    \* detach, its waker still stored.
    if HandleOp \in {"give_up", "poll_detach"} /\ ~gaveUp then
        gaveUp := TRUE;
        if HandleOp = "give_up" then goto H_cancel; else goto H_detach; end if;
    end if;
H_sleep:
    \* The handle's executor sleeps until the waker it polled with is woken, or polls it again
    \* without a wake, which a future must allow. A waker it no longer polls with wakes it no
    \* more.
    either
        await cur \in woken;
        woken := woken \ {cur};
        parkWoken := TRUE;                   \* (record)
    or
        await spurious > 0;
        spurious := spurious - 1;
    end either;
H_rewake:
    \* With two wakers, the executor may poll it next with the other one.
    if HandleWakers = 2 then
        either skip; or cur := 3 - cur; end either;
    end if;
    goto H_poll;
H_detach:
    \* `Task::detach`, then `set_detached`. The common case right after the spawn is one
    \* exchange, `detach`'s inline one. (`Task::drop`
    \* that finds the task completed, closed and with no reference left takes the free of
    \* H_det_loop at once, without the exchange: nothing else can reach the word then.)
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
        destroy("handle");
        goto H_det_push;
    else
        tsk := FALSE;
        goto H_det_push;
    end if;
H_det_read:
    \* The output is read out, to be dropped once the handle is gone (`cancel_and_let_go`): a
    \* panic outcome is logged there; a value goes to the handle's dropper, whose own panic, if
    \* its drop panics, is theirs. MUTATION detach_panic_silent: the panic goes unlogged.
    touch();
    useOutput("dropped");
    if hookFate = "outcome" then
        hookFate := IF Mutation = "detach_panic_silent" THEN "silent" ELSE "told";
    elsif dropFate = "outcome" then
        dropFate := IF Mutation = "detach_panic_silent" THEN "silent" ELSE "told";
    end if;
    goto H_det_loop;
H_det_push:
    if dPush then touch(); queue := queue + 1; end if;
end process;

(***************************************************************************)
(* The abort handle (`AbortHandle`, made by `Task::abort_handle` right     *)
(* after the spawn): a clone of a waker of the task, so one reference of   *)
(* it, held on a thread of its own, which may outlive the handle. It       *)
(* aborts the task (`AbortHandle::abort`, the `cancel` the handle's own    *)
(* cancel runs), then is dropped: a waker's drop, whose last reference of  *)
(* an open task closes it and schedules it once more, so the future is     *)
(* dropped where the task runs.                                            *)
(***************************************************************************)
fair process aborter \in Abort
variables aPrevAwt = FALSE, aPush = FALSE;
begin
A_abort:
    \* `cancel`: close it unless it completed or is closed already; an idle task is scheduled
    \* once more, with a reference for its runnable.
    touch();
    if ~(comp \/ clo) then
        aPrevAwt := awt;
        aborted := TRUE;                     \* (record)
        clo := TRUE;
        if ~(sch \/ run) then
            sch := TRUE;
            refs := refs + 1;
            aPush := TRUE;
        end if;
    end if;
A_push:
    \* The runnable goes to the run queue.
    if aPush then touch(); queue := queue + 1; end if;
A_notify:
    \* A stored waker is taken (`notify(None)`) ...
    if aPrevAwt then call Take(FALSE); end if;
A_wake:
    \* ... and woken.
    if took[self] # 0 then wakeAwaiter(took[self]); took[self] := 0; end if;
A_drop:
    \* The abort handle is dropped: its reference goes (`drop_waker`).
    \* MUTATION abort_no_ref: it holds none, so the abort above may touch freed memory, and
    \* its drop lets go of nothing.
    if Mutation # "abort_no_ref" then dropWaker(); end if;
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

\* The future is dropped at most once, and only by a thread that holds a runnable of it: a
\* runner, or the waker thread whose push found the shard stopped (a shard's task is `Send`;
\* a bound task's push goes to its home, which drops nothing, so a bound task's future is
\* dropped by its home thread alone). Never by the handle.
FutureOnce == futureDrops <= 1 /\ futureDropper \in {"nobody", "runner", "waker"}

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

\* A handle that awaits a task nobody cancels (the shutdown did not drop its runnable) gets its
\* output: the value, or the panic of the task's own code.
AwaitGetsOutput == (HandleOp = "await" /\ pc[HandleT] = "Done" /\ ~shutDropped /\ ~aborted)
                       => handleGot \in {"output", "panic"}

\* The run never frees a task whose handle held it as it completed: the handle's drop does,
\* on the handle's thread, so a task's memory goes back where its awaiter runs before the
\* awaiter goes on (a blocking job's block is back in its spawner's cache before the
\* spawner spawns again: tests/zero_alloc.rs).
RunnerFreesNoAwaited == freer = "runner" => ~awaitedAtComplete

\* A runner starts a run only on a task that is scheduled and not running: what lets the
\* run's first exchange be one addition that turns SCHEDULED into RUNNING.
RunnableScheduled == \A r \in Runners : pc[r] = "R_start" => (sch /\ ~run)

\* A handle that answers None does so once the future is gone: a cancel that returns has
\* nothing of the task's future left running.
CancelWaits == handleGot = "none" => future = "dropped"

\* A handle that answers None does so once the output is gone too: a cancel that returns has
\* nothing of the task's output left alive.
OutputGoneAtNone == handleGot = "none" => output # "out"

\* A waker the handle stored is let go of by the time the memory goes: taken and woken (or,
\* the handle's own, dropped), or dropped with the memory. One left in the cell would never be
\* dropped: a leak of the awaiter's own task.
AwaiterOnce == freed => slot = 0

\* Never two threads in the awaiter cell at once: a take reaches it only while no
\* registration holds it.
OneAtTheCell == ~cellRace

\* Where every panic of the task's own code went, one entry each.
Fates == <<hookFate, dropFate, valueFate, freeFate, shutFate, wakerFate>>

\* No panic of the task's own code aborts the process.
NoAbort == \A i \in 1..6 : Fates[i] # "aborted"

\* Once every thread is done, every panic of the task's own code was told: resumed in the
\* handle that awaited the task, or logged.
PanicTold == AllDone => \A i \in 1..6 : Fates[i] \in {"none", "told"}

\* The task's own code panicked: a hook around its poll, or its future's drop.
TaskPanicked == hookFate # "none" \/ dropFate # "none"

\* A handle that awaits the task, or cancels it and waits for its future to go, gets the panic
\* of the task's own code as the task's outcome.
AwaiterGetsPanic == (HandleOp \in {"await", "cancel"} /\ pc[HandleT] = "Done" /\ TaskPanicked)
                        => handleGot = "panic"

\* All the safety properties together.
Safe == OnePoll /\ FutureOnce /\ NoPollAfterDrop /\ NothingAfterFree /\ OutputOnce
        /\ FreedAfterFuture /\ CleanEnd /\ AwaitGetsOutput /\ RunnerFreesNoAwaited
        /\ RunnableScheduled /\ CancelWaits /\ OutputGoneAtNone /\ AwaiterOnce /\ OneAtTheCell

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
\* A cancel ends as a completion with a panic, for the handle.
NoUnclose == ~unclosed
\* A runnable is dropped without being run.
NoShutDrop == ~shutDropped
\* The waker thread's push finds the shard stopped and drops the task's future itself.
NoWakerDrop == futureDropper # "waker"
\* The abort handle closes the task.
NoAborted == ~aborted
\* A registration finds a notification in progress, with a waker stored that is not the one
\* the handle polls with now.
NoStaleStored == ~staleMet
\* The handle's own cancel (its drop) takes the waker it stored in an earlier poll.
NoCancelNotify == ~cancelNotified
\* The completion's claim, decided on a stale word, finds the awaiter cell empty.
NoClaimEmpty == ~claimedEmpty
\* A clone of the handle's waker panics as the handle registers it.
NoClonePanic == ~clonePanicked
=============================================================================

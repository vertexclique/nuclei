------------------------------ MODULE FileLock ------------------------------
(***************************************************************************)
(* A file's lock taken without blocking a thread (src/fs/lock.rs:          *)
(* `lock_file`), at the grain of its atomic steps.                         *)
(*                                                                          *)
(* In plain words: `lock_file` asks the kernel for the file's lock with a  *)
(* call that never waits (std's `try_lock`: `flock` with `LOCK_NB`, or     *)
(* `LockFileEx` with `LOCKFILE_FAIL_IMMEDIATELY` on Windows), on the       *)
(* caller's own thread. Free, the file is locked and the poll that asked   *)
(* returns the lock to its caller, in one step. Held by another, the       *)
(* future sleeps on a timer, a little longer each time up to a bound, and  *)
(* asks again when it fires. The call may also fail, and then nothing is   *)
(* locked. Since the ask and the return are one step, a future dropped at  *)
(* any moment it can be dropped (while it sleeps) holds nothing: there is  *)
(* nothing to give back. No thread of the blocking pool takes part, so a   *)
(* lock another process keeps for good keeps no thread: a write of the     *)
(* same file, a job of the pool, still runs.                               *)
(*                                                                          *)
(* Who acts: the caller's future, the timer it sleeps on, another process  *)
(* that holds the lock at first (with `Outsider`) and may never let go,    *)
(* and a write of the same file made as a job of a pool of `Threads`       *)
(* threads once the lock's future is gone (with `Write`).                  *)
(*                                                                          *)
(* What must never happen: once the caller is done, the file is locked     *)
(* although no caller holds the lock (`NoOwnerlessLock`), which nobody     *)
(* would ever give back. What must happen: the write runs (`WriteServed`), *)
(* and once the other process let go the caller ends (`EndsOnceFree`).    *)
(*                                                                          *)
(* The mutation `blocking_on_pool` is the code before: the lock is a       *)
(* blocking call, a job of the pool that waits in the kernel on a pool     *)
(* thread until the file is free. Dropping the future cannot reach the     *)
(* job there, so a lock kept for good keeps the thread for good, and with  *)
(* one thread the write never runs.                                        *)
(*                                                                          *)
(* What is abstract: the kernel's lock is one flag (`locked`), the other   *)
(* process another (`outside`); the timer's length and its backoff (the   *)
(* timer fires at some moment after it is set); the pool is a count of     *)
(* free threads.                                                           *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    CanFail,   \* TRUE: the call may fail (then nothing is locked); FALSE: it locks a free file
    Outsider,  \* TRUE: another process holds the file's lock at first, and may never let it go
    Threads,   \* the threads of the blocking pool
    Write,     \* TRUE: a write of the same file is made as a job of the pool once the future is gone
    Mutation   \* "none", or the name of the one rule this run takes away

(* --algorithm FileLock
variables
    locked = FALSE,         \* the file is locked for the caller in the kernel right now
    returned = FALSE,       \* the future handed the lock to its caller
    outside = Outsider,     \* the other process holds the file's lock now
    armed = FALSE,          \* the future's timer is set and has not fired
    fired = FALSE,          \* the timer fired: the sleeping future asks again
    retried = FALSE,        \* (record) a call found the lock held and the future asked again
    free = Threads,         \* threads of the pool that run no job
    job = "none",           \* the blocking job of the mutation: "none", "waiting", "done"
    dropped = FALSE,        \* the future was dropped before it took the lock
    written = FALSE;        \* the write job ran

define
    \* The caller is done: it returned, failed, or was dropped (a finished process sits at Done).
    CallerDone == pc[1] = "Done"
    \* Once the caller is done (and no job of the mutation still waits), a locked file means the
    \* caller holds the lock.
    NoOwnerlessLock == (CallerDone /\ job # "waiting") => (locked => returned)
    \* A lock handed to the caller really is held.
    ReturnedIsHeld == returned => locked
    \* Once the lock's future is gone, the write of the file made then runs in the end.
    WriteServed == (Write /\ dropped) ~> written
    \* Once the other process let go, the caller ends.
    EndsOnceFree == (~outside) ~> CallerDone
    \* Witness: a call finds the lock held, and the future asks again once its timer fired.
    NoRetry == ~retried
end define;

fair process caller = 1                 \* the future of the `lock_file` call
begin
C_try:
    \* MUTATION blocking_on_pool: the lock is a blocking call on a thread of the pool.
    if Mutation = "blocking_on_pool" then
        await free > 0;                 \* the job takes a thread of the pool
        free := free - 1;
        job := "waiting";               \* and waits in the kernel
        goto C_job;
    elsif ~outside /\ ~locked then      \* one poll: the call that never waits finds the file free
        locked := TRUE;                 \* the kernel locks it
        if Mutation = "try_apart" then  \* MUTATION try_apart: the answer is returned in a later step
            goto C_return;
        else
            returned := TRUE;           \* the caller holds it now, in the same step
            goto Done;
        end if;
    else
        either
            await CanFail;              \* the call fails (only in the runs that allow it)
            goto Done;                  \* its error goes to the caller, and nothing is locked
        or
            \* Held by another: the future sets its timer, a step longer each time, and sleeps.
            \* MUTATION no_timer: it sleeps with no timer set, to be woken by nothing.
            armed := (Mutation # "no_timer");
            fired := FALSE;
        end either;
    end if;
C_sleep:
    either
        await fired /\ ~dropped;        \* the timer fired: ask again
        fired := FALSE;
        retried := TRUE;
        goto C_try;
    or
        await dropped;                  \* the future was dropped while it slept: it holds nothing
        armed := FALSE;                 \* its timer goes with it
        goto Done;
    end either;
C_return:
    if ~dropped then                    \* the later step returns the lock (the mutation only),
        returned := TRUE;               \* unless the future was dropped before it
    end if;
    goto Done;
C_job:
    either
        await job = "done" /\ ~dropped; \* the job got the lock: the future takes it
        returned := TRUE;
    or
        await dropped;                  \* the future was dropped; its job stays in the kernel
    end either;
end process;

\* The caller's own code: it may drop the future wherever it waits, or never. Not fair: a drop is
\* the caller's choice, never forced.
process user = 6
begin
U_drop:
    await pc[1] \in {"C_sleep", "C_return", "C_job"};
    dropped := TRUE;
end process;

fair process timer = 2                  \* the future's timer
begin
T_fire:
    while TRUE do
        await armed;                    \* once it is set
        armed := FALSE;                 \* it fires at some moment
        fired := TRUE;
    end while;
end process;

fair process pooljob = 3                \* the mutation's blocking job on a thread of the pool
begin
J_lock:
    await job = "waiting" /\ ~outside;  \* it waits in the kernel until the other process let go
    locked := ~dropped;                 \* it locks; with nobody left to want it, it unlocks at once
    job := "done";                      \* its end is there for the future
    free := free + 1;                   \* and its thread is the pool's again
end process;

process outsider = 4                    \* the other process; not fair: it may never let go
begin
O_let_go:
    if outside then                     \* it holds the lock
O_unlock:
        outside := FALSE;               \* and lets go, if it ever does
    end if;
end process;

fair process writer = 5                 \* a write of the same file, as a job of the pool
begin
W_submit:
    if Write then                       \* in the runs that have one
        await dropped;                  \* made once the lock's future is gone
W_run:
        await free > 0;                 \* it waits for a thread of the pool
        free := free - 1;               \* runs on it
W_done:
        written := TRUE;                \* writes
        free := free + 1;               \* and gives the thread back
    end if;
end process;
end algorithm; *)
=============================================================================

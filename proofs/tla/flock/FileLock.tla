------------------------------ MODULE FileLock ------------------------------
(***************************************************************************)
(* The cancellation of a file's blocking lock (src/fs/lock.rs: `lock` and  *)
(* its `LockCall`), at the grain of its atomic steps.                      *)
(*                                                                          *)
(* In plain words: `File::lock` waits in the kernel until the file is      *)
(* free, so nuclei makes it as a job of the blocking pool. Two parties     *)
(* act: the caller's future, and the pool's job. They share one little     *)
(* word of state, `st`, which says WAITING (the job has not got the lock   *)
(* yet), HELD (the job got the lock and the future has not been dropped)   *)
(* or GONE (the future was dropped). The job locks the file, and then      *)
(* does one compare-and-swap from WAITING to HELD. If it finds GONE        *)
(* instead, nobody wants the lock any more, so the job unlocks the file    *)
(* itself. The future, when it is dropped before it handed the lock to its *)
(* caller, swaps GONE into the word in one step; if the word was HELD, the *)
(* lock is the future's to give back, so it unlocks the file at once. A    *)
(* job that has not started when the future is dropped is cancelled and    *)
(* never locks. The job's lock may also fail, and then there is nothing    *)
(* to give back.                                                           *)
(*                                                                          *)
(* What must never happen: once the caller and the job are both done, the  *)
(* file is locked although no caller holds the lock (`NoOwnerlessLock`).   *)
(* Such a lock could never be given back, and every other process would    *)
(* wait for it for good. What must happen: both parties always end         *)
(* (`EveryoneEnds`).                                                       *)
(*                                                                          *)
(* What is modelled step by step: the shared word and each swap or         *)
(* compare-and-swap on it, the job's claim (start or cancel), the kernel's *)
(* lock and each unlock, the future's drop at any point before it takes    *)
(* the result (its swap, its inline unlock and its cancel of the job as    *)
(* separate steps), and the future taking the result. What is abstract:    *)
(* the kernel's lock is one flag (`locked`), because one file and one      *)
(* call are enough to show who unlocks; other holders of the file, and the *)
(* time the blocking lock waits, are the kernel's.                         *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    CanFail,   \* TRUE: the job's blocking lock may fail (then nothing is locked); FALSE: it always gets the lock
    Mutation   \* "none", or the name of the one rule this run takes away

(* --algorithm FileLock
variables
    st = "WAITING",         \* the shared word: "WAITING", "HELD" or "GONE"
    locked = FALSE,         \* the file is locked in the kernel right now
    jobState = "queued",    \* the pool's job: "queued", "running", "cancelled" or "done"
    jobOk = FALSE,          \* the job's blocking lock succeeded
    returned = FALSE,       \* the future handed the lock to its caller
    lockAfterGone = FALSE,  \* record: the job got the lock when the word already said GONE
    dropUnlocked = FALSE;   \* record: a dropped future found HELD and unlocked the file itself

define
    \* Both parties finished (a finished process sits at the label Done).
    AllDone == \A p \in DOMAIN pc : pc[p] = "Done"
    \* Once both are done, a locked file means a caller really holds the lock.
    NoOwnerlessLock == AllDone => (locked => returned)
    \* A lock handed to the caller really is held.
    ReturnedIsHeld == returned => locked
    \* Witness: the job can get the lock after its future was dropped.
    NoLockAfterGone == ~lockAfterGone
    \* Witness: a dropped future can find the lock held and give it back itself.
    NoDropUnlock == ~dropUnlocked
    \* Both parties always end.
    EveryoneEnds == <>AllDone
end define;

fair process caller = 1                 \* the future of the `lock` call
variables old = "WAITING";              \* what the drop's swap found in the word
begin
C_poll:
    either                              \* the future is dropped now (before or after the job ended)
        goto C_swap;                    \* go and run the drop
    or                                  \* or the future is polled once the job ended
        await jobState = "done";        \* it takes the result only when the job is done
        if jobOk then                   \* the job got the lock
            returned := TRUE;           \* the caller now holds it
        end if;                         \* (a failed lock returns its error and holds nothing)
        goto Done;                      \* a returned future is not dropped as an owner any more
    end either;
C_swap:
    if Mutation = "drop_reads_then_writes" then   \* the broken drop: reads the word...
        old := st;                      \* ...in one step...
    else                                \* the real drop: one atomic swap
        old := st;                      \* it reads the word...
        st := "GONE";                   \* ...and writes GONE in the same step
    end if;
C_store:
    if Mutation = "drop_reads_then_writes" then   \* the broken drop: ...and writes later
        st := "GONE";                   \* a job's step in between is overwritten
    end if;
C_unlock:
    if old = "HELD" /\ Mutation # "drop_ignores_held" then   \* the job's lock was waiting for the caller
        locked := FALSE;               \* the future gives it back at once
        dropUnlocked := TRUE;           \* record that this happened
    end if;
C_cancel:
    if jobState = "queued" then         \* the job has not started
        jobState := "cancelled";        \* dropping its task cancels it; it never locks
    end if;                             \* a running job is left to finish by itself
end process;

fair process job = 2                    \* the job of the blocking pool
begin
J_begin:
    if jobState = "queued" then         \* the pool thread claims the job...
        jobState := "running";          \* ...and it runs
    else                                \* it was cancelled before it started
        goto Done;                      \* it never locks
    end if;
J_lock:
    either                              \* the blocking lock succeeds
        locked := TRUE;                 \* the file is locked
        jobOk := TRUE;                  \* the job says so
        if st = "GONE" then             \* the future was dropped in the meantime
            lockAfterGone := TRUE;      \* record it
        end if;
    or                                  \* or it fails
        await CanFail;                  \* only in the runs that allow a failure
        jobOk := FALSE;                 \* nothing is locked
    end either;
J_cas:
    if jobOk then                       \* the job holds a lock to hand over
        if Mutation = "job_ignores_gone" then   \* the broken job: never looks for GONE
            st := "HELD";               \* it writes HELD over whatever is there
            goto J_end;                 \* and ends
        elsif st = "WAITING" then       \* the real job: compare-and-swap from WAITING...
            st := "HELD";               \* ...to HELD, the future will hand it over or give it back
            goto J_end;                 \* and ends
        else                            \* the word says GONE: nobody wants the lock
            goto J_unlock;              \* so the job gives it back itself
        end if;
    end if;                             \* a failed lock goes on to the end
J_end:
    jobState := "done";                 \* the result is there for the future
    goto Done;                          \* the job is finished
J_unlock:
    locked := FALSE;                    \* the job unlocks the file (not blocking)
    goto J_end;                         \* and ends
end process;
end algorithm; *)
=============================================================================

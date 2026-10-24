/-!
# A task's references, future and output, one operation at a time

This file checks the bookkeeping of a task of nuclei's runtime (`src/rt/task/`): one
allocation holding a future, a state word of flags and a count of references, reached through
a runnable, any number of wakers and one handle. It follows `Nuclei/Oneshot.lean`: the model,
the invariant and the theorems in one file, for one thread doing one whole operation at a time.

## The story in plain words

Spawning makes a task with its handle and one runnable. Running the runnable polls the future
once. A poll can end Ready (the future is dropped and its output stored for the handle, or
dropped at once when no handle is left), or Pending; during a Pending poll the future may clone
its waker, drop one of its wakers, or wake itself (then the run hands the task back to its
queue). Every waker is a reference; a wake makes a runnable for an idle task. The handle can
take the output, cancel the task (an idle one is scheduled once more, so its future is dropped
by a run), or let go (detach). The memory is freed by whoever lets go of the last reference
once the handle is gone, and never before the future and the output are gone.

A worked example, computed below (`example_detached_last_waker`): spawn, run (Pending, the
future keeps a waker), detach, wake by reference (a new runnable), run again with the future
dropping that waker during the poll: the run's reference is the last one, nobody can wake the
task or await it, so the run drops the future and frees the memory. (This is the case the
established task crate leaves the future behind in.)

## What the model leaves out

It is one thread doing one whole operation at a time, so a poll is never seen in progress and
the RUNNING flag never shows: a wake during a poll is one of the poll's outcomes here. The
TLA+ model in `proofs/tla/task` checks the interleavings, including wakes, cancels and drops
that land during a poll on other threads. Here the waker count is unbounded: the theorems hold
for any number of clones. The awaiter's waker cell and its handshake are left to TLA+ too.
-/

namespace Nuclei.Task

/-! ## The state

Every field is a natural number; the flags are 0 or 1, so the facts below are linear and
`omega` proves them. -/

/-- Everything the task and the world around it know between two operations. -/
structure St where
  /-- SCHEDULED: a runnable exists. -/
  sched : Nat := 1
  /-- COMPLETED: the future returned Ready. -/
  comp : Nat := 0
  /-- CLOSED: cancelled, output taken, or completed with no handle. -/
  clo : Nat := 0
  /-- TASK: the handle exists. -/
  tsk : Nat := 1
  /-- The count of references in the state word. -/
  refs : Nat := 1
  /-- (ghost) Runnables alive. -/
  runnables : Nat := 1
  /-- (ghost) Wakers alive. -/
  wakers : Nat := 0
  /-- The future is alive. -/
  fut : Nat := 1
  /-- (ghost) How many times the future was dropped. -/
  futDrops : Nat := 0
  /-- The output is stored, waiting for the handle. -/
  out : Nat := 0
  /-- (ghost) How many times the output was taken or dropped. -/
  outUses : Nat := 0
  /-- The memory is freed. -/
  freed : Nat := 0
  /-- (ghost) How many times it was freed. -/
  frees : Nat := 0
deriving DecidableEq, Repr

/-- A task just spawned: scheduled, with its handle and its runnable.
Example: `spawned.refs = 1` and `spawned.wakers = 0`. -/
def spawned : St := {}

/-- How a poll ends. -/
inductive Poll where
  /-- Ready: the future is done. -/
  | ready
  /-- Pending, nothing else. -/
  | pending
  /-- Pending, after cloning its waker (to store it somewhere). -/
  | pendingClone
  /-- Pending, after dropping one of its wakers (needs a waker to exist). -/
  | pendingDrop
  /-- Pending, after waking itself: the run schedules it again. -/
  | pendingWoken
deriving DecidableEq, Repr

/-- One whole operation. -/
inductive Op where
  /-- Clone a waker (from a waker, or from the runnable). -/
  | clone
  /-- `wake_by_ref`. -/
  | wakeByRef
  /-- `wake`, which spends the waker. -/
  | wake
  /-- Drop a waker. -/
  | dropWaker
  /-- Run the runnable, the poll ending as given. -/
  | run (p : Poll)
  /-- Drop the runnable without running it. -/
  | dropRunnable
  /-- The handle cancels (`set_canceled`). -/
  | cancel
  /-- The handle takes the output of a completed task. -/
  | take
  /-- The handle lets go (`set_detached`; dropping a handle is a cancel, then this). -/
  | detach
deriving DecidableEq, Repr

/-! ## The operations

Each operation's `if`s come first and each branch is one record update, so the proofs below
split every case and check it with linear arithmetic. -/

/-- `drop_ref`, by a run or a runnable that holds a reference: the last reference with no
handle frees the task, dropping the future first if the task is still open (nothing can wake
or await it any more); otherwise the count goes down. -/
def dropRef (s : St) : St :=
  if s.refs = 1 ∧ s.tsk = 0 then
    if s.comp = 0 ∧ s.clo = 0 then
      { s with refs := 0, fut := 0, futDrops := s.futDrops + 1, freed := 1,
               frees := s.frees + 1 }
    else { s with refs := 0, freed := 1, frees := s.frees + 1 }
  else { s with refs := s.refs - 1 }

/-- `drop_waker`: as `dropRef`, but the last waker of a task still open closes it and
schedules it once more (the new runnable keeps the reference), so a run drops its future. -/
def dropWakerRef (s : St) : St :=
  if s.refs = 1 ∧ s.tsk = 0 then
    if s.comp = 0 ∧ s.clo = 0 then
      { s with sched := 1, clo := 1, runnables := s.runnables + 1 }
    else { s with refs := 0, freed := 1, frees := s.frees + 1 }
  else { s with refs := s.refs - 1 }

/-- One operation, when the one doing it holds what it needs (a waker, the runnable, the
handle); otherwise nothing happens. -/
def step (s : St) : Op → St
  | .clone =>
    if s.wakers + s.runnables > 0 then
      { s with wakers := s.wakers + 1, refs := s.refs + 1 }
    else s
  | .wakeByRef =>
    if s.wakers > 0 then
      if s.comp = 1 ∨ s.clo = 1 ∨ s.sched = 1 then s
      else { s with sched := 1, refs := s.refs + 1, runnables := s.runnables + 1 }
    else s
  | .wake =>
    if s.wakers > 0 then
      if s.comp = 1 ∨ s.clo = 1 ∨ s.sched = 1 then dropWakerRef { s with wakers := s.wakers - 1 }
      -- The waker's reference becomes the runnable's.
      else { s with wakers := s.wakers - 1, sched := 1, runnables := s.runnables + 1 }
    else s
  | .dropWaker =>
    if s.wakers > 0 then dropWakerRef { s with wakers := s.wakers - 1 } else s
  | .run p =>
    if s.runnables > 0 then
      if s.clo = 1 then
        -- Cancelled in the queue: drop the future, unschedule, drop the run's reference.
        dropRef { s with fut := 0, futDrops := s.futDrops + 1, sched := 0, runnables := 0 }
      else
        match p with
        | .ready =>
          -- The future is dropped and its output stored; with no handle the output is
          -- dropped at once and the task closed.
          if s.tsk = 0 then
            dropRef { s with fut := 0, futDrops := s.futDrops + 1, sched := 0, comp := 1,
                             runnables := 0, clo := 1, outUses := s.outUses + 1 }
          else
            dropRef { s with fut := 0, futDrops := s.futDrops + 1, sched := 0, comp := 1,
                             runnables := 0, out := 1 }
        | .pending => dropRef { s with sched := 0, runnables := 0 }
        | .pendingClone =>
          dropRef { s with sched := 0, runnables := 0, wakers := s.wakers + 1,
                           refs := s.refs + 1 }
        | .pendingDrop =>
          if s.wakers > 0 then
            -- The run holds its own reference, so this drop is never the last.
            dropRef { s with sched := 0, runnables := 0, wakers := s.wakers - 1,
                             refs := s.refs - 1 }
          else s
        -- Woken during the poll: the run's reference goes to the new runnable.
        | .pendingWoken => s
    else s
  | .dropRunnable =>
    if s.runnables > 0 then
      -- Closed unless done, then the future goes and the reference.
      if s.comp = 0 ∧ s.clo = 0 then
        dropRef { s with clo := 1, fut := 0, futDrops := s.futDrops + 1, sched := 0,
                         runnables := 0 }
      else
        dropRef { s with fut := 0, futDrops := s.futDrops + 1, sched := 0, runnables := 0 }
    else s
  | .cancel =>
    if s.tsk = 1 then
      if s.comp = 1 ∨ s.clo = 1 then s
      else if s.sched = 0 then
        { s with sched := 1, clo := 1, refs := s.refs + 1, runnables := s.runnables + 1 }
      else { s with clo := 1 }
    else s
  | .take =>
    if s.tsk = 1 ∧ s.comp = 1 ∧ s.clo = 0 then
      { s with clo := 1, out := 0, outUses := s.outUses + 1 }
    else s
  | .detach =>
    if s.tsk = 1 then
      if s.comp = 1 ∧ s.clo = 0 then
        -- The output nobody took is read out and dropped; the task is closed.
        if s.refs = 0 then
          { s with clo := 1, out := 0, outUses := s.outUses + 1, tsk := 0, freed := 1,
                   frees := s.frees + 1 }
        else { s with clo := 1, out := 0, outUses := s.outUses + 1, tsk := 0 }
      else if s.refs = 0 then
        if s.clo = 0 then
          -- The last reference with the task open: close it, schedule it once more.
          { s with sched := 1, clo := 1, refs := 1, runnables := s.runnables + 1, tsk := 0 }
        else { s with tsk := 0, freed := 1, frees := s.frees + 1 }
      else { s with tsk := 0 }
    else s

/-- The state after a whole sequence of operations. -/
def run (s : St) : List Op → St
  | [] => s
  | op :: ops => run (step s op) ops

/-! ## The invariant -/

/-- What holds of every state a spawned task can reach, while its memory lives. -/
def Live (s : St) : Prop :=
  -- The flags are flags.
  s.sched ≤ 1 ∧ s.comp ≤ 1 ∧ s.clo ≤ 1 ∧ s.tsk ≤ 1 ∧ s.fut ≤ 1 ∧ s.out ≤ 1 ∧
  -- Not freed yet.
  s.freed = 0 ∧ s.frees = 0 ∧
  -- The count is exact: the runnable and the wakers, nothing else.
  s.refs = s.runnables + s.wakers ∧
  -- At most one runnable, and SCHEDULED says whether it exists.
  s.sched = s.runnables ∧
  -- The future is alive, or was dropped exactly once.
  s.fut + s.futDrops = 1 ∧
  -- A runnable always has a live future, so its run or drop is the future's one drop.
  s.sched ≤ s.fut ∧
  -- A completed task has no future (and so no runnable), and an open one still has it:
  -- every path that drops the future completes, closes or frees the task.
  s.fut + s.comp ≤ 1 ∧ s.fut + s.comp + s.clo ≥ 1 ∧
  -- A cancelled task whose future is still alive has a runnable that will drop it.
  s.fut + s.clo ≤ 1 + s.runnables ∧
  -- The output is stored, or was used exactly once, exactly when the task completed.
  s.out + s.outUses = s.comp ∧
  -- A stored output waits for a handle that has not closed the task, and a completed task
  -- not closed yet still holds it (closing takes or drops it).
  s.out ≤ s.tsk ∧ s.out + s.clo ≤ 1 ∧ s.comp ≤ s.out + s.clo ∧
  -- Memory nobody refers to has been freed: here something still does.
  s.refs + s.tsk ≥ 1

/-- What holds once the memory is freed: nothing is left. -/
def Dead (s : St) : Prop :=
  s.freed = 1 ∧ s.frees = 1 ∧ s.refs = 0 ∧ s.runnables = 0 ∧ s.wakers = 0 ∧ s.sched = 0 ∧
  s.tsk = 0 ∧ s.fut = 0 ∧ s.futDrops = 1 ∧ s.out = 0 ∧ s.outUses ≤ 1

/-- Every reachable state: alive and accounted, or freed with nothing left. -/
def Good (s : St) : Prop := Live s ∨ Dead s

/-- A spawned task is good. -/
theorem spawned_good : Good spawned := by
  left; simp [Live, spawned]

/-! ## Every operation keeps the invariant -/

/-- A freed task is inert: every operation needs a reference or the handle, and none is left. -/
theorem dead_step (s : St) (op : Op) (d : Dead s) : step s op = s := by
  unfold Dead at d
  obtain ⟨-, -, -, hrun, hw, -, ht, -⟩ := d
  cases op with
  | run p => simp [step, hrun]
  | _ => simp [step, hw, hrun, ht]

set_option linter.unusedSimpArgs false in
/-- Every operation keeps a good state good. Each operation's `if`s are split and every
branch is checked by linear arithmetic, as a live state (`Live`) or a freed one (`Dead`).
(The operations unfold different helpers, so not every case uses each name given to `simp`.) -/
theorem step_good (s : St) (op : Op) (g : Good s) : Good (step s op) := by
  rcases g with l | d
  · unfold Live at l
    rcases op with _ | _ | _ | _ | (_ | _ | _ | _ | _) | _ | _ | _ | _ <;>
      simp only [step, dropRef, dropWakerRef] <;>
      (repeat' split) <;>
      simp only [Good] <;>
      first
        | (left; simp only [Live, true_and, and_true] at *; omega)
        | (right; simp only [Dead, true_and, and_true] at *; omega)
  · rw [dead_step s op d]; exact Or.inr d

/-- Every state a spawned task reaches, through any sequence of operations, is good. -/
theorem run_good (ops : List Op) : Good (run spawned ops) := by
  suffices h : ∀ s, Good s → Good (run s ops) from h spawned spawned_good
  induction ops with
  | nil => intro s g; exact g
  | cons op ops ih => intro s g; exact ih (step s op) (step_good s op g)

/-! ## What the invariant says, one theorem each -/

/-- The future is dropped at most once. -/
theorem future_dropped_once (ops : List Op) : (run spawned ops).futDrops ≤ 1 := by
  rcases run_good ops with l | d <;> simp only [Live, Dead] at * <;> omega

/-- The output is taken or dropped at most once. -/
theorem output_used_once (ops : List Op) : (run spawned ops).outUses ≤ 1 := by
  rcases run_good ops with l | d <;> simp only [Live, Dead] at * <;> omega

/-- The memory is freed at most once, and only after the future and the output are gone. -/
theorem freed_once_and_last (ops : List Op) :
    (run spawned ops).frees ≤ 1 ∧
      ((run spawned ops).freed = 1 →
        (run spawned ops).fut = 0 ∧ (run spawned ops).out = 0) := by
  rcases run_good ops with l | d <;> simp only [Live, Dead] at * <;> omega

/-- Freed memory is never touched again: with no reference and no handle left, every
operation finds nothing to do it with and changes nothing. -/
theorem freed_is_inert (ops : List Op) (op : Op) (h : (run spawned ops).freed = 1) :
    step (run spawned ops) op = run spawned ops := by
  rcases run_good ops with l | d
  · simp only [Live] at l; omega
  · exact dead_step _ op d

/-- No leak: a task nobody refers to and nobody holds a handle of is freed. -/
theorem no_leak (ops : List Op) (hr : (run spawned ops).refs = 0)
    (ht : (run spawned ops).tsk = 0) : (run spawned ops).freed = 1 := by
  rcases run_good ops with l | d <;> simp only [Live, Dead] at * <;> omega

/-! ## The worked example -/

/-- Spawn, run (Pending, the future keeps a waker), detach, wake by reference, run again with
the future dropping that waker: the run drops the future and frees the memory. -/
theorem example_detached_last_waker :
    run spawned [.run .pendingClone, .detach, .wakeByRef, .run .pendingDrop] =
      { sched := 0, comp := 0, clo := 0, tsk := 0, refs := 0, runnables := 0, wakers := 0,
        fut := 0, futDrops := 1, out := 0, outUses := 0, freed := 1, frees := 1 } := by
  decide

end Nuclei.Task

/-!
# The single-shard semaphore, one operation at a time

This file checks the bookkeeping of `sync::local::Semaphore` (`src/sync/local/semaphore.rs`,
its line in `src/sync/local/waiters.rs`), which `sync::local::Mutex` and the bounded
`sync::local::mpsc` channel's slots are built on: a strictly first-come, first-served
semaphore for the tasks of one shard.

## The story in plain words

The semaphore has a number of free permits and a line of waiting requests, oldest first. A
request takes its permits at once when nobody waits and they are free; otherwise it joins the
back of the line. A release gives permits back and serves the line: the front request is
granted when its whole need fits what is free, then the next, and so on, stopping at the first
that does not fit, so nobody behind it is served first, however little it asks for. An attempt
that never waits takes nothing while anybody waits. A dropped request leaves the line; when it
was the front, the line is served again, so the next one is not left waiting with its permits
free. A granted request that is dropped before it took its grant gives the permits back, which
is a release.

A worked example, computed below: 2 permits, a request for 2 takes both; a request for 2 and
then one for 1 wait; the holder releases 2: the front (2) is granted, and the request for 1
still waits, with nothing free.

## What the model leaves out

One whole operation at a time: the code runs on one thread, and every operation is one step of
it, as here. Wakers, the slab the line lives in and the close (which ends every waiter at once
and serves nobody) are not in this model; the TLA+ model in `proofs/tla/shardlocal/` checks the
same rules with the tasks' polls interleaved, and the waiters' drops at any point.
-/

namespace Nuclei.LocalSemaphore

/-- A request waiting in line: its arrival number and how many permits it asks for. -/
structure Waiter where
  /-- The arrival number. -/
  id : Nat
  /-- The permits it asks for. -/
  need : Nat
deriving DecidableEq

/-- The permits a list of waiters asks for, together. -/
def needs : List Waiter → Nat
  | [] => 0
  | w :: rest => w.need + needs rest

/-- The pass (`Semaphore::serve`): with `free` permits, the front waiters whose needs fit, one
after another, are granted. Answers the permits left, the waiters granted and the line left. -/
def serve (free : Nat) : List Waiter → Nat × List Waiter × List Waiter
  | [] => (free, [], [])
  | w :: rest =>
    if w.need ≤ free then
      ((serve (free - w.need) rest).1, w :: (serve (free - w.need) rest).2.1,
        (serve (free - w.need) rest).2.2)
    else (free, [], w :: rest)

/-- The pass grants the front of the line, in its order, and leaves the rest: nobody is served
ahead of a waiter that came before it. -/
theorem serve_grants_the_front (free : Nat) (line : List Waiter) :
    (serve free line).2.1 ++ (serve free line).2.2 = line := by
  induction line generalizing free with
  | nil => simp [serve]
  | cons w rest ih =>
    by_cases h : w.need ≤ free
    · simp only [serve]
      rw [ite_eq_left h]
      simp only [List.cons_append, ih]
    · simp only [serve]
      rw [ite_eq_right h]
      simp

/-- The pass loses no permit and makes none: what it leaves free and what it granted add up to
what was free. -/
theorem serve_conserves (free : Nat) (line : List Waiter) :
    (serve free line).1 + needs (serve free line).2.1 = free := by
  induction line generalizing free with
  | nil => simp [serve, needs]
  | cons w rest ih =>
    by_cases h : w.need ≤ free
    · have := ih (free - w.need)
      simp only [serve]
      rw [ite_eq_left h]
      simp only [needs]
      omega
    · simp only [serve]
      rw [ite_eq_right h]
      simp [needs]

/-- The pass stops at a front that does not fit: after it, the line is empty or its front needs
more than is free. -/
theorem serve_leaves_an_unfit_front (free : Nat) (line : List Waiter) (w : Waiter)
    (rest : List Waiter) (h : (serve free line).2.2 = w :: rest) : (serve free line).1 < w.need := by
  induction line generalizing free with
  | nil => simp [serve] at h
  | cons v more ih =>
    by_cases fit : v.need ≤ free
    · simp only [serve] at h ⊢
      rw [ite_eq_left fit] at h ⊢
      exact ih (free - v.need) h
    · simp only [serve] at h ⊢
      rw [ite_eq_right fit] at h ⊢
      show free < w.need
      simp only [List.cons.injEq] at h
      rw [← h.1]
      omega

/-! ## The operations -/

/-- Everything the semaphore knows between two operations. -/
structure Sem where
  /-- The permits it holds in all; never changes. -/
  cap : Nat
  /-- Permits free. -/
  free : Nat
  /-- Permits held, by holders and by granted requests that have not taken theirs yet. -/
  held : Nat
  /-- The waiting requests, oldest first. -/
  line : List Waiter
  /-- The arrival number the next request gets. -/
  nextId : Nat

/-- A fresh semaphore of `cap` permits. -/
def fresh (cap : Nat) : Sem := { cap := cap, free := cap, held := 0, line := [], nextId := 0 }

/-- One operation. -/
inductive Op where
  /-- A request for `need` permits that waits if it must (`acquire_many`). -/
  | acquire (need : Nat)
  /-- A request that never waits (`try_acquire_many`). -/
  | tryAcquire (need : Nat)
  /-- `n` held permits are given back: a permit dropped, `add_permits` of held ones, or a
  granted request dropped before it took its grant. -/
  | release (n : Nat)
  /-- The waiting request with this arrival number is dropped. -/
  | cancel (id : Nat)

/-- The line served with `free` permits: the granted requests hold theirs. -/
def served (s : Sem) (free : Nat) (line : List Waiter) : Sem :=
  { s with free := (serve free line).1, held := s.held + needs (serve free line).2.1,
           line := (serve free line).2.2 }

/-- Whether `id` is the front waiter's. -/
def isFront (id : Nat) : List Waiter → Bool
  | [] => false
  | w :: _ => w.id == id

/-- One whole operation (`Acquire::poll`, `try_acquire_many`, `release`, `Acquire::cancel`). -/
def step (s : Sem) : Op → Sem
  | .acquire need =>
    if s.line = [] ∧ need ≤ s.free then
      { s with free := s.free - need, held := s.held + need, nextId := s.nextId + 1 }
    else { s with line := s.line ++ [⟨s.nextId, need⟩], nextId := s.nextId + 1 }
  | .tryAcquire need =>
    if s.line = [] ∧ need ≤ s.free then
      { s with free := s.free - need, held := s.held + need, nextId := s.nextId + 1 }
    else { s with nextId := s.nextId + 1 }
  | .release n =>
    if n ≤ s.held then served { s with held := s.held - n } (s.free + n) s.line else s
  | .cancel id =>
    if isFront id s.line then served s s.free (s.line.filter (fun w => w.id != id))
    else { s with line := s.line.filter (fun w => w.id != id) }

/-- A run of operations from a state. -/
def run (s : Sem) : List Op → Sem
  | [] => s
  | op :: ops => run (step s op) ops

/-- The invariant: no permit lost or made up, and no waiter left waiting while its whole need is
free at the front of the line. -/
def Good (s : Sem) : Prop :=
  s.free + s.held = s.cap ∧ ∀ w rest, s.line = w :: rest → s.free < w.need

theorem fresh_good (cap : Nat) : Good (fresh cap) := by
  unfold Good fresh
  simp

/-- Serving any line keeps the count, and leaves an unfit front. -/
theorem served_good (s : Sem) (free : Nat) (line : List Waiter) (count : free + s.held = s.cap) :
    Good (served s free line) := by
  have c := serve_conserves free line
  refine ⟨?_, ?_⟩
  · simp only [served]
    omega
  · intro w rest h
    simp only [served] at h ⊢
    exact serve_leaves_an_unfit_front free line w rest h

/-- Dropping a waiter that is not the front keeps the front where it was. -/
theorem filter_keeps_the_front (id : Nat) (w : Waiter) (rest : List Waiter)
    (h : isFront id (w :: rest) = false) :
    (w :: rest).filter (fun v => v.id != id) = w :: rest.filter (fun v => v.id != id) := by
  simp only [isFront, beq_eq_false_iff_ne] at h
  simp [h]

/-- Every operation keeps the invariant. -/
theorem step_good (s : Sem) (op : Op) (g : Good s) : Good (step s op) := by
  obtain ⟨count, front⟩ := g
  cases op with
  | acquire need =>
    simp only [step]
    by_cases take : s.line = [] ∧ need ≤ s.free
    · rw [ite_eq_left take]
      have := take.2
      refine ⟨by simp only; omega, ?_⟩
      intro w rest h
      simp only at h
      rw [take.1] at h
      simp at h
    · rw [ite_eq_right take]
      refine ⟨by simp only; omega, ?_⟩
      intro w rest h
      simp only at h ⊢
      cases e : s.line with
      | nil =>
        have : ¬ need ≤ s.free := fun h' => take ⟨e, h'⟩
        rw [e] at h
        simp only [List.nil_append, List.cons.injEq] at h
        rw [← h.1]
        show s.free < need
        omega
      | cons v more =>
        rw [e] at h
        simp only [List.cons_append, List.cons.injEq] at h
        rw [← h.1]
        exact front v more e
  | tryAcquire need =>
    simp only [step]
    by_cases take : s.line = [] ∧ need ≤ s.free
    · rw [ite_eq_left take]
      have := take.2
      refine ⟨by simp only; omega, ?_⟩
      intro w rest h
      simp only at h
      rw [take.1] at h
      simp at h
    · rw [ite_eq_right take]
      exact ⟨by simp only; omega, fun w rest h => front w rest h⟩
  | release n =>
    simp only [step]
    by_cases ok : n ≤ s.held
    · rw [ite_eq_left ok]
      exact served_good _ _ _ (by simp only; omega)
    · rw [ite_eq_right ok]
      exact ⟨count, front⟩
  | cancel id =>
    simp only [step]
    by_cases f : isFront id s.line = true
    · rw [ite_eq_left f]
      exact served_good _ _ _ count
    · rw [ite_eq_right f]
      refine ⟨by simp only; omega, ?_⟩
      intro w rest h
      simp only at h ⊢
      cases e : s.line with
      | nil =>
        rw [e] at h
        simp at h
      | cons v more =>
        rw [e] at h f
        rw [filter_keeps_the_front id v more (by simpa using f)] at h
        simp only [List.cons.injEq] at h
        rw [← h.1]
        exact front v more e

/-- Every run of operations from a fresh semaphore keeps the invariant. -/
theorem run_good (cap : Nat) (ops : List Op) : Good (run (fresh cap) ops) := by
  suffices ∀ s, Good s → Good (run s ops) from this _ (fresh_good cap)
  induction ops with
  | nil => intro s g; exact g
  | cons op ops ih =>
    intro s g
    simp only [run]
    exact ih _ (step_good s op g)

/-- The worked example: 2 permits; a request for 2 takes both; requests for 2 and for 1 wait;
the holder releases 2: the front is granted, and the request for 1 still waits. -/
theorem example_front_first :
    let s := run (fresh 2) [.acquire 2, .acquire 2, .acquire 1, .release 2]
    s.free = 0 ∧ s.held = 2 ∧ s.line.map (·.need) = [1] := by
  decide

end Nuclei.LocalSemaphore

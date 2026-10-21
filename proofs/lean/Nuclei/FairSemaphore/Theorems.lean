import Nuclei.FairSemaphore.Invariant

/-!
# What the fair semaphore's sequential model proves

The model is in `Model.lean` and the invariant in `Invariant.lean`. This file proves, for
every sequence of operations (acquire, try-acquire, release, cancel, close) from a fresh
semaphore:

1. `conservation`: the free permits, the set-aside and the permits held add up to the
   semaphore's size, always;
2. `waiting_means_nothing_free` and `queue_in_arrival_order`: while anybody waits, no permit
   is free (so an attempt that does not wait can take nothing), and the line is ordered by
   arrival;
3. `no_overtaking` and `fresh_only_when_nobody_waits`: a step grants waiters only from the
   front of the line, so a request that arrived later is never granted while an earlier one
   still waits, and a request granted without waiting found the line empty;
4. `cancel_passes_share_on`: when the front waiter is dropped, the whole share set aside for
   it goes to the next waiter (which is granted, or holds that share and waits for the rest);
5. `closed_admits_nothing`: after the close, no request is ever granted again.
-/

namespace Nuclei.FairSemaphore

/-! ## The theorems -/

/-- **Conservation.** After any sequence of operations on a fresh semaphore of `cap` permits,
the free permits, the set-aside and the permits held add up to `cap`: no permit is ever lost
or made up, whatever is acquired, released, cancelled or closed. -/
theorem conservation (cap : Nat) (ops : List Op) :
    (run (fresh cap) ops).free + (run (fresh cap) ops).setAside
      + total (run (fresh cap) ops).holding = cap := by
  -- Every reachable state is good ...
  have g := run_good (fresh cap) ops (fresh_good cap)
  -- ... so its permits add up to its size ...
  have := g.conserve
  -- ... which is the size it started with.
  rw [run_cap] at this
  exact this

/-- **Nothing is free while somebody waits.** So an attempt that does not wait takes nothing
from a waiter: it can only succeed when the line is empty (or it asks for nothing). -/
theorem waiting_means_nothing_free (cap : Nat) (ops : List Op)
    (h : (run (fresh cap) ops).queue ≠ []) : (run (fresh cap) ops).free = 0 := by
  -- Every reachable state is good.
  have g := run_good (fresh cap) ops (fresh_good cap)
  -- Look at the line.
  cases hq : (run (fresh cap) ops).queue with
  -- Empty: but somebody waits, so this case cannot be.
  | nil => exact absurd hq h
  -- Somebody at the front: then nothing is free, by the invariant.
  | cons w rest => exact (g.front w rest hq).2

/-- **The line is in arrival order** after any sequence of operations (it is part of the
invariant every reachable state keeps). -/
theorem queue_in_arrival_order (cap : Nat) (ops : List Op) :
    (run (fresh cap) ops).queue.Pairwise (fun a b => a.id < b.id) :=
  (run_good (fresh cap) ops (fresh_good cap)).sorted

/-- In a line ordered by arrival that a pass split into a granted prefix and the rest, a waiter
that arrived before a granted one was granted too. -/
theorem earlier_granted {g r : List Waiter} {a b : Waiter}
    (hs : (g ++ r).Pairwise (fun a b => a.id < b.id)) (hb : b ∈ g) (ha : a ∈ g ++ r)
    (hlt : a.id < b.id) : a ∈ g := by
  -- Everything granted came before everything left, in arrival order.
  rw [List.pairwise_append] at hs
  simp only [List.mem_append] at ha
  -- `a` is granted, or left in line.
  rcases ha with ha | ha
  · exact ha
  · -- Left in line: then `b` (granted) arrived before `a`, against `a` arriving first.
    have := hs.2.2 b hb a ha
    omega

/-- The pass on a line ordered by arrival grants nobody that arrived after a waiter it leaves. -/
theorem pass_no_overtaking (s : Sem) (hs : s.queue.Pairwise (fun a b => a.id < b.id))
    {a b : Waiter} (ha : a ∈ s.queue) (hb : b ∈ (pass s).2) (hlt : a.id < b.id) :
    a ∈ (pass s).2 := by
  -- The line is the granted prefix followed by the rest.
  have hp := serve_prefix s.free s.setAside s.queue
  simp only [pass] at hb ⊢
  rw [← hp] at hs ha
  -- So the previous fact applies.
  exact earlier_granted hs hb ha hlt

/-- **No overtaking.** In every step from a reachable state, a waiter granted from the line was
not granted ahead of anybody who arrived before it and still waited: each of those was granted
in the same step (before it, in the order of the grant list), unless the step was that
waiter's own cancellation. -/
theorem no_overtaking (s : Sem) (g : Good s) (op : Op) {a b : Waiter}
    (ha : a ∈ s.queue) (hb : b ∈ (step s op).granted) (hlt : a.id < b.id) :
    a ∈ (step s op).granted ∨ op = .cancel a.id := by
  -- One case per operation.
  cases op with
  | acquire need =>
    -- An acquire never cancels anybody: show `a` was granted.
    left
    by_cases hclosed : s.closed = true
    -- Closed: nobody is granted, so `hb` cannot be.
    · simp [step, hclosed] at hb
    · by_cases hfresh : need = 0 ∨ (s.queue = [] ∧ need ≤ s.free)
      -- Granted at once: nobody is granted from the line, so `hb` cannot be.
      · simp [step, hclosed, hfresh] at hb
      · -- Queued: the pass ran on the line with the newcomer at the back.
        simp only [step, hclosed, hfresh, ite_false, passStep] at hb ⊢
        refine pass_no_overtaking _ (sorted_append g.sorted g.ids) ?_ hb hlt
        -- `a` was in the line, so it is in the longer one.
        simp [ha]
  | tryAcquire need =>
    -- A try-acquire grants nobody from the line.
    simp only [step] at hb
    split at hb <;> simp at hb
  | release id =>
    -- A release cancels nobody: show `a` was granted.
    left
    -- Was there such a holder?
    cases hh : takeHolder id s.holding with
    -- No: nobody is granted.
    | none => simp [step, hh] at hb
    | some p =>
      -- Yes: the pass ran on the same line.
      obtain ⟨n, rest⟩ := p
      simp only [step, hh, passStep] at hb ⊢
      exact pass_no_overtaking { s with free := s.free + n, holding := rest } g.sorted ha hb hlt
  | cancel id =>
    -- Was there such a waiter?
    cases hq : dropWaiter id s.queue with
    -- No: nobody is granted.
    | none => simp [step, hq] at hb
    | some q =>
      -- `a` is still in line, or it is the waiter this step dropped.
      rcases mem_dropWaiter hq ha with ha' | hid
      · -- Still in line: the pass ran on the shorter line, which is still ordered.
        left
        simp only [step, hq, passStep] at hb ⊢
        exact pass_no_overtaking _ (g.sorted.sublist (dropWaiter_sublist hq)) ha' hb hlt
      · -- Dropped: this step was its own cancellation.
        right
        rw [hid]
  | close =>
    -- The close grants nobody.
    simp [step] at hb

/-- **A request granted without waiting overtakes nobody**: it asked for nothing, or the line
was empty. -/
theorem fresh_only_when_nobody_waits (s : Sem) (op : Op) {w : Waiter}
    (h : (step s op).fresh = some w) : w.need = 0 ∨ s.queue = [] := by
  -- One case per operation; only the two acquires grant at once.
  cases op with
  | acquire need =>
    simp only [step] at h
    -- Closed: nothing is granted at once.
    split at h
    · simp at h
    · -- Granted at once exactly when it asks for nothing or nobody waits.
      split at h
      · next hfresh =>
        simp only [Option.some.injEq] at h
        subst h
        rcases hfresh with h0 | ⟨hq, _⟩
        · exact Or.inl h0
        · exact Or.inr hq
      -- Queued: nothing is granted at once.
      · simp [passStep] at h
  | tryAcquire need =>
    simp only [step] at h
    split at h
    · -- Granted at once exactly when open and it asks for nothing or nobody waits.
      next hfresh =>
      simp only [Option.some.injEq] at h
      subst h
      rcases hfresh.2 with h0 | ⟨hq, _⟩
      · exact Or.inl h0
      · exact Or.inr hq
    -- Refused: nothing is granted.
    · simp at h
  -- A release, a cancel and the close grant nobody at once.
  | release id =>
    simp only [step] at h
    split at h <;> simp [passStep] at h
  | cancel id =>
    simp only [step] at h
    split at h <;> simp [passStep] at h
  | close => simp [step] at h

/-- **A dropped front waiter passes its share on.** When the front waiter `w` of a reachable
state is dropped and `v` is next in line, then after the step either `v` is granted, or `v`
is at the front holding the whole share that was set aside for `w`. -/
theorem cancel_passes_share_on (s : Sem) (g : Good s) (w v : Waiter) (rest : List Waiter)
    (hq : s.queue = w :: v :: rest) :
    v ∈ (step s (.cancel w.id)).granted ∨
      ((step s (.cancel w.id)).next.queue = v :: rest ∧
        (step s (.cancel w.id)).next.setAside = s.setAside) := by
  -- Nothing is free while w waits.
  have hfree := (g.front w (v :: rest) hq).2
  -- Dropping w takes it off the front.
  have hdrop : dropWaiter w.id s.queue = some (v :: rest) := by
    rw [hq]; simp [dropWaiter]
  -- Unfold the step: the pass runs on the line that starts with v.
  simp only [step, hdrop, passStep, pass]
  rw [hfree]
  -- With nothing free, v is granted from the share alone, or keeps it.
  by_cases hc : v.need ≤ s.setAside
  · -- The share covers v: v is granted.
    left
    simp [serve, hc]
  · -- It does not: v stays at the front with the whole share.
    right
    simp [serve, hc]

/-- From a closed state, no step grants anybody, and the semaphore stays closed. -/
theorem closed_step (s : Sem) (g : Good s) (hc : s.closed = true) (op : Op) :
    (step s op).granted = [] ∧ (step s op).fresh = none ∧ (step s op).next.closed = true ∧
      (step s op).next.holding.Sublist s.holding := by
  -- A closed semaphore has nobody in line.
  have h0 := g.closedEmpty hc
  -- One case per operation.
  cases op with
  -- The acquires fail at once.
  | acquire need => simp [step, hc]
  | tryAcquire need => simp [step, hc]
  | release id =>
    simp only [step]
    split
    -- No such holder: nothing changes.
    · simp [hc]
    · -- A holder gives back: the pass over an empty line grants nobody.
      next n rest hh =>
      simp only [passStep, pass, h0, serve]
      refine ⟨by simp, by simp, by simpa using hc, ?_⟩
      -- And the holding list only lost that holder.
      simpa using takeHolder_sublist hh
  | cancel id =>
    simp only [step]
    split
    -- No such waiter: nothing changes.
    · simp [hc]
    -- There is nobody to drop from an empty line.
    · next q hq => rw [h0] at hq; simp [dropWaiter] at hq
  -- Closing again changes nothing that matters here.
  | close => simp [step]

/-- **The close admits nothing.** Once closed, whatever operations follow, the semaphore stays
closed and the requests holding permits are only ever fewer: none is ever granted again. -/
theorem closed_admits_nothing (s : Sem) (g : Good s) (hc : s.closed = true) (ops : List Op) :
    (run s ops).closed = true ∧ (run s ops).holding.Sublist s.holding := by
  -- By induction on the operations.
  induction ops generalizing s with
  -- No operation: nothing changed.
  | nil => exact ⟨hc, List.Sublist.refl _⟩
  | cons op ops ih =>
    -- The first step keeps it closed and only drops holders ...
    obtain ⟨_, _, hc', hsub⟩ := closed_step s g hc op
    -- ... and so do the rest, from the good state the first step left.
    obtain ⟨hc'', hsub'⟩ := ih (step s op).next (step_good s op g) hc'
    exact ⟨hc'', hsub'.trans hsub⟩

/-! ## The worked example of the module docs, computed

`decide` runs the model on the example and checks the result, as a test would; the three
statements below are about this one run only. -/

/-- The example: 3 permits; request 0 takes 3, requests 1 (for 2) and 2 (for 1) wait, request 0
gives its permits back, request 3 asks for 3, request 1 gives back, request 4 asks for 1,
request 3 is dropped. -/
def example_ops : List Op :=
  [.acquire 3, .acquire 2, .acquire 1, .release 0, .acquire 3, .release 1, .acquire 1,
   .cancel 3]

/-- Step 3 of the example: after request 0's release, requests 1 and 2 hold their permits and
nothing is free or set aside. -/
theorem example_after_release :
    let s := run (fresh 3) (example_ops.take 4)
    s.free = 0 ∧ s.setAside = 0 ∧ s.queue = [] ∧ s.holding = [(1, 2), (2, 1)] := by
  decide

/-- Step 4: request 3 (for 3) holds a share of 2 and request 4 (for 1) waits behind it, with
nothing free. -/
theorem example_share_held :
    let s := run (fresh 3) (example_ops.take 7)
    s.free = 0 ∧ s.setAside = 2 ∧ s.queue.map (·.id) = [3, 4] := by
  decide

/-- Step 5: request 3 is dropped; request 4 is granted from the share, and the share's other
permit goes back to the free count. -/
theorem example_after_cancel :
    let s := run (fresh 3) example_ops
    s.free = 1 ∧ s.setAside = 0 ∧ s.queue = [] ∧ s.holding = [(4, 1), (2, 1)] := by
  decide

end Nuclei.FairSemaphore

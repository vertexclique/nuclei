import Nuclei.FairSemaphore.Model

/-!
# The fair semaphore's invariant

This file proves the facts the pieces obey on their own (the holding list, the line, the
pass), and then the invariant they add up to: what holds of every state a fresh semaphore
can reach, and how the pass, every operation and whole sequences of operations keep it.
The definitions are in `Model.lean`; `Theorems.lean` states what a whole run gives.
-/

namespace Nuclei.FairSemaphore

/-! ## Facts about the pieces

A note on reading the proofs. Each line after `by` is one step that Lean checks. `induction l`
proves a fact about every list by proving it for the empty list (`nil`) and for a list with one
more entry in front (`cons`), assuming it for the shorter list (`ih`). `simp` rewrites with the
definitions it is given until the goal is obviously true; `omega` finishes any goal that is
plain arithmetic on whole numbers; `split` looks at both branches of an `if` or a `match`;
`exact` hands over a proof that fits the goal as it stands. -/

/-- Adding up two holding lists one after the other is adding their totals. -/
theorem total_append (a b : List (Nat × Nat)) : total (a ++ b) = total a + total b := by
  -- By induction on the first list.
  induction a with
  -- Nothing in front: both sides are `total b`.
  | nil => simp [total]
  | cons x xs ih =>
    -- One entry in front adds its count on both sides.
    obtain ⟨j, n⟩ := x
    -- Unfold `total` once and use the fact for the shorter list.
    simp [total, ih]
    -- What is left is `n + (s + t) = n + s + t`.
    omega

/-- The holding list of granted waiters holds exactly what they asked for. -/
theorem total_map (l : List Waiter) : total (l.map (fun w => (w.id, w.need))) = needs l := by
  -- By induction on the list of waiters.
  induction l with
  -- No waiter: both are 0.
  | nil => rfl
  -- One more waiter adds its need on both sides.
  | cons w ws ih => simp [total, needs, ih]

/-- Taking a holder out takes exactly its permits out of the total. -/
theorem takeHolder_total {id n : Nat} {l rest : List (Nat × Nat)}
    (h : takeHolder id l = some (n, rest)) : total l = n + total rest := by
  -- By induction on the holding list, for every `rest`.
  induction l generalizing rest with
  -- An empty list has no holder to take: `h` cannot be.
  | nil => simp [takeHolder] at h
  | cons x xs ih =>
    -- Name the first entry's number and count.
    obtain ⟨j, m⟩ := x
    -- Unfold one step of `takeHolder` in `h`.
    simp only [takeHolder] at h
    -- Is the first entry the holder?
    split at h
    · -- The first entry is the holder: its count is the one taken.
      simp only [Option.some.injEq, Prod.mk.injEq] at h
      -- So `n` is its count and `rest` is the tail.
      obtain ⟨rfl, rfl⟩ := h
      -- And `total` of the whole list is that count plus the tail's.
      simp [total]
    · -- Otherwise the holder is further down, and this entry stays.
      cases hx : takeHolder id xs with
      -- Not found further down either: `h` cannot be.
      | none => simp [hx] at h
      | some r =>
        -- Found further down: name what was taken there.
        obtain ⟨m', r'⟩ := r
        -- `h` now says the result keeps this entry in front of what came back.
        simp only [hx, Option.map_some, Option.some.injEq, Prod.mk.injEq] at h
        obtain ⟨rfl, rfl⟩ := h
        -- The fact for the shorter list.
        have := ih hx
        -- Unfold `total` on both sides ...
        simp [total]
        -- ... and the arithmetic follows.
        omega

/-- A holder taken out leaves a list that only lost entries. -/
theorem takeHolder_sublist {id n : Nat} {l rest : List (Nat × Nat)}
    (h : takeHolder id l = some (n, rest)) : rest.Sublist l := by
  -- By induction on the holding list, as above.
  induction l generalizing rest with
  -- Nothing to take from an empty list.
  | nil => simp [takeHolder] at h
  | cons x xs ih =>
    -- Name the first entry.
    obtain ⟨j, m⟩ := x
    -- Unfold one step of `takeHolder`.
    simp only [takeHolder] at h
    -- Is the first entry the holder?
    split at h
    · -- Yes: `rest` is the tail ...
      simp only [Option.some.injEq, Prod.mk.injEq] at h
      obtain ⟨rfl, rfl⟩ := h
      -- ... which is the whole list without its first entry.
      exact List.Sublist.cons _ (List.Sublist.refl _)
    · -- No: look at what happened further down.
      cases hx : takeHolder id xs with
      -- Not found: `h` cannot be.
      | none => simp [hx] at h
      | some r =>
        -- Found: the result keeps this entry in front of a sublist of the tail.
        obtain ⟨m', r'⟩ := r
        simp only [hx, Option.map_some, Option.some.injEq, Prod.mk.injEq] at h
        obtain ⟨rfl, rfl⟩ := h
        -- Keeping the same first entry on both sides keeps it a sublist.
        exact List.Sublist.cons_cons _ (ih hx)

/-- A waiter taken out of the line leaves a line that only lost a waiter. -/
theorem dropWaiter_sublist {id : Nat} {l q : List Waiter} (h : dropWaiter id l = some q) :
    q.Sublist l := by
  -- By induction on the line, for every result `q`.
  induction l generalizing q with
  -- Nothing to drop from an empty line.
  | nil => simp [dropWaiter] at h
  | cons w ws ih =>
    -- Unfold one step of `dropWaiter`.
    simp only [dropWaiter] at h
    -- Is the first waiter the one dropped?
    split at h
    · -- Yes: the result is the rest of the line ...
      simp only [Option.some.injEq] at h
      subst h
      -- ... which is the line without its first waiter.
      exact List.Sublist.cons _ (List.Sublist.refl _)
    · -- No: look further down.
      cases hx : dropWaiter id ws with
      -- Not found: `h` cannot be.
      | none => simp [hx] at h
      | some r =>
        -- Found: the first waiter stays in front of a sublist of the rest.
        simp only [hx, Option.map_some, Option.some.injEq] at h
        subst h
        exact List.Sublist.cons_cons _ (ih hx)

/-- The pass loses no permit: what was free or set aside is still free, set aside, or held by
the waiters it granted. -/
theorem serve_conserves (free setAside : Nat) (q : List Waiter) :
    (serve free setAside q).free + (serve free setAside q).setAside
      + needs (serve free setAside q).granted = free + setAside := by
  -- By induction on the line, for every free count and set-aside.
  induction q generalizing free setAside with
  -- Nobody in line: the set-aside joins the free count, nothing is granted.
  | nil => simp [serve, needs]
  | cons w ws ih =>
    -- Unfold one step of the pass.
    simp only [serve]
    -- Was the front covered?
    split
    · -- Granted: the front's need moves to `granted`, the rest is passed on.
      have := ih (free - min free (w.need - setAside)) (setAside + min free (w.need - setAside) - w.need)
      -- Add the front's need to what the rest of the pass granted.
      simp only [needs]
      -- And the counts add up, since the front was covered.
      omega
    · -- Not covered: the permits only moved from free to the set-aside.
      simp [needs]
      omega

/-- The pass grants a prefix of the line, oldest first, and the rest stays in line in the
same order: nobody is skipped. -/
theorem serve_prefix (free setAside : Nat) (q : List Waiter) :
    (serve free setAside q).granted ++ (serve free setAside q).rest = q := by
  -- By induction on the line.
  induction q generalizing free setAside with
  -- Nobody in line: nothing granted, nothing left.
  | nil => simp [serve]
  | cons w ws ih =>
    -- Unfold one step of the pass.
    simp only [serve]
    -- Was the front covered?
    split
    -- Yes: the front leads the granted list, and the rest follows by the fact for the rest.
    · simp [ih]
    -- No: nothing granted, the whole line stays.
    · simp

/-- After a pass, nobody left in line means nothing is set aside. -/
theorem serve_empty (free setAside : Nat) (q : List Waiter)
    (h : (serve free setAside q).rest = []) : (serve free setAside q).setAside = 0 := by
  -- By induction on the line.
  induction q generalizing free setAside with
  -- Nobody in line: the pass gave the set-aside back.
  | nil => simp [serve]
  | cons w ws ih =>
    -- Unfold one step of the pass, in `h` and in the goal.
    simp only [serve] at h ⊢
    -- Was the front covered?
    split
    · -- Yes: the rest of the pass decides, by the fact for the rest.
      next hc =>
      simp only [hc, ite_true] at h
      exact ih _ _ h
    · -- No: then the front is still in line, so `h` (an empty line) cannot be.
      next hc => simp [hc] at h

/-- After a pass, a waiter left at the front has less set aside than it needs, and no permit
is free: every free permit went to it. -/
theorem serve_front (free setAside : Nat) (q : List Waiter) (w : Waiter) (rest : List Waiter)
    (h : (serve free setAside q).rest = w :: rest) :
    (serve free setAside q).setAside < w.need ∧ (serve free setAside q).free = 0 := by
  -- By induction on the line.
  induction q generalizing free setAside with
  -- An empty line leaves nobody at the front: `h` cannot be.
  | nil => simp [serve] at h
  | cons v vs ih =>
    -- Unfold one step of the pass.
    simp only [serve] at h ⊢
    -- Was the front covered?
    split
    · -- Yes: the waiter left at the front is further down; the rest of the pass decides.
      next hc =>
      simp only [hc, ite_true] at h
      exact ih _ _ h
    · -- No: this front is `w`, and it stays.
      next hc =>
      simp only [hc, ite_false, List.cons.injEq] at h
      obtain ⟨rfl, rfl⟩ := h
      simp only
      -- It took `min free lack` and is still short, so it took every free permit. Each
      -- half separately: `omega` over the whole conjunction would lean on `Classical.choice`.
      exact ⟨by omega, by omega⟩

/-! ## The invariant -/

/-- What holds of every state a fresh semaphore can reach. -/
structure Good (s : Sem) : Prop where
  /-- Conservation: every permit is free, set aside, or held. -/
  conserve : s.free + s.setAside + total s.holding = s.cap
  /-- The line is ordered by arrival. -/
  sorted : s.queue.Pairwise (fun a b => a.id < b.id)
  /-- Every waiter has an arrival number already handed out. -/
  ids : ∀ w ∈ s.queue, w.id < s.nextId
  /-- Nobody in line: nothing set aside. -/
  empty : s.queue = [] → s.setAside = 0
  /-- Somebody in line: the front is not covered yet, and nothing is free. -/
  front : ∀ w rest, s.queue = w :: rest → s.setAside < w.need ∧ s.free = 0
  /-- A closed semaphore has nobody in line. -/
  closedEmpty : s.closed = true → s.queue = []

/-- A fresh semaphore is good: every part is true of all free, an empty line and nothing
held (`simp` checks each one from the definition of `fresh`). -/
theorem fresh_good (cap : Nat) : Good (fresh cap) where
  conserve := by simp [fresh, total]
  sorted := by simp [fresh]
  ids := by simp [fresh]
  empty := by simp [fresh]
  front := by simp [fresh]
  closedEmpty := by simp [fresh]

/-- A line ordered by arrival stays ordered when a waiter newer than all of it joins the back. -/
theorem sorted_append {q : List Waiter} {w : Waiter}
    (hs : q.Pairwise (fun a b => a.id < b.id)) (hlt : ∀ v ∈ q, v.id < w.id) :
    (q ++ [w]).Pairwise (fun a b => a.id < b.id) := by
  -- A joined list is ordered when both parts are, and everything in the first comes before
  -- everything in the second.
  rw [List.pairwise_append]
  -- The line is ordered, and a single waiter is ordered; what is left is the last part.
  refine ⟨hs, by simp, ?_⟩
  -- Take any `a` from the line and the newcomer `b`.
  intro a ha b hb
  simp only [List.mem_singleton] at hb
  subst hb
  -- The newcomer is newer than everybody in line.
  exact hlt a ha

/-! ## Every step keeps the invariant -/

/-- A waiter of a line that a sublist was taken from is a waiter of the line. -/
theorem mem_of_sublist {a : Waiter} {l m : List Waiter} (h : l.Sublist m) (ha : a ∈ l) : a ∈ m :=
  h.subset ha

/-- The pass keeps a state good: it needs only that the permits add up, that the line is
ordered and numbered, and that a closed semaphore has nobody in line. -/
theorem pass_good (s : Sem) (hc : s.free + s.setAside + total s.holding = s.cap)
    (hs : s.queue.Pairwise (fun a b => a.id < b.id)) (hi : ∀ w ∈ s.queue, w.id < s.nextId)
    (he : s.closed = true → s.queue = []) : Good (pass s).1 := by
  -- The pass splits the line into the granted prefix and the rest.
  have hp := serve_prefix s.free s.setAside s.queue
  -- So what stays in line is a sublist of the line before.
  have hsub : (serve s.free s.setAside s.queue).rest.Sublist s.queue := by
    conv => rhs; rw [← hp]
    exact List.sublist_append_right _ _
  -- Prove each part of `Good` in turn.
  refine
    { conserve := ?_, sorted := ?_, ids := ?_, empty := ?_, front := ?_, closedEmpty := ?_ }
  · -- Conservation: what the pass granted is now held by the granted waiters.
    have := serve_conserves s.free s.setAside s.queue
    simp only [pass, total_append, total_map]
    omega
  · -- The rest of a line ordered by arrival is ordered by arrival.
    exact hs.sublist hsub
  · -- Its waiters were in the line, so their numbers were handed out.
    intro w hw
    exact hi w (mem_of_sublist hsub hw)
  · -- Nobody left in line: the set-aside went back to the free count.
    exact serve_empty s.free s.setAside s.queue
  · -- Somebody left at the front: not covered, and nothing free.
    intro w rest h
    exact serve_front s.free s.setAside s.queue w rest h
  · -- Closed: the line was empty, so the pass leaves it empty.
    intro h
    simp only [pass]
    rw [he h]
    simp [serve]

/-- A waiter is still in the line after `dropWaiter id`, unless it is the one with number `id`. -/
theorem mem_dropWaiter {id : Nat} {a : Waiter} {l q : List Waiter}
    (h : dropWaiter id l = some q) (ha : a ∈ l) : a ∈ q ∨ a.id = id := by
  -- By induction on the line.
  induction l generalizing q with
  -- Nobody is in an empty line: `ha` cannot be.
  | nil => simp at ha
  | cons w ws ih =>
    -- Unfold one step of `dropWaiter`.
    simp only [dropWaiter] at h
    -- Was the first waiter the one dropped?
    split at h
    · -- Yes: the result is the rest of the line.
      next hw =>
      simp only [Option.some.injEq] at h
      subst h
      simp only [List.mem_cons] at ha
      -- `a` is either that first waiter (then it has number `id`) or in the rest.
      rcases ha with rfl | ha
      · exact Or.inr hw
      · exact Or.inl ha
    · -- No: the first waiter stays; look further down.
      cases hx : dropWaiter id ws with
      -- Not found further down: `h` cannot be.
      | none => simp [hx] at h
      | some r =>
        simp only [hx, Option.map_some, Option.some.injEq] at h
        subst h
        simp only [List.mem_cons] at ha ⊢
        -- `a` is the first waiter, which stayed, or it is further down: use the fact there.
        rcases ha with rfl | ha
        · exact Or.inl (Or.inl rfl)
        · rcases ih hx ha with h1 | h1
          · exact Or.inl (Or.inr h1)
          · exact Or.inr h1

/-- Every operation keeps a good state good. -/
theorem step_good (s : Sem) (op : Op) (g : Good s) : Good (step s op).next := by
  -- One case per operation.
  cases op with
  | acquire need =>
    -- Unfold the step.
    simp only [step]
    -- Is the semaphore closed?
    split
    · -- Closed: only the arrival number moves on, and it only grows.
      exact { conserve := g.conserve, sorted := g.sorted,
              ids := fun w hw => Nat.lt_succ_of_lt (g.ids w hw), empty := g.empty,
              front := g.front, closedEmpty := g.closedEmpty }
    · next hclosed =>
      -- Open: is the request granted at once?
      split
      · -- Granted at once: it asks for nothing, or nobody waits and enough is free.
        next hfresh =>
        -- Only the free count and the holding list change; check conservation and the front.
        refine { conserve := ?_, sorted := g.sorted,
                 ids := fun w hw => Nat.lt_succ_of_lt (g.ids w hw), empty := g.empty,
                 front := ?_, closedEmpty := g.closedEmpty }
        · -- The permits taken out of the free count are now held.
          have := g.conserve
          simp only [total]
          rcases hfresh with h0 | ⟨_, hle⟩
          -- Asking for nothing changes nothing.
          · subst h0; simp only [Nat.sub_zero]; omega
          -- Taking `need` from at least `need` free permits.
          · omega
        · -- A waiter at the front keeps its facts: asking for nothing took nothing, and
          -- otherwise the line was empty.
          intro w rest hq
          have := g.front w rest hq
          rcases hfresh with h0 | ⟨hq0, _⟩
          · subst h0; simp only [Nat.sub_zero]; exact this
          · rw [hq0] at hq; simp at hq
      · -- It joins the back of the line, and the pass runs.
        apply pass_good
        -- The permits did not move.
        · exact g.conserve
        -- The newcomer has the newest number, so the line stays ordered.
        · exact sorted_append g.sorted g.ids
        -- Every number in the longer line was handed out, the newcomer's last.
        · intro w hw
          simp only [List.mem_append, List.mem_singleton] at hw
          rcases hw with hw | rfl
          · exact Nat.lt_succ_of_lt (g.ids w hw)
          · exact Nat.lt_succ_self _
        -- The semaphore is open, so the closed case cannot arise.
        · intro h
          exact absurd h hclosed
  | tryAcquire need =>
    -- Unfold the step.
    simp only [step]
    -- Granted at once, or not?
    split
    · -- Granted at once: as for an acquire granted at once.
      next hfresh =>
      refine { conserve := ?_, sorted := g.sorted,
               ids := fun w hw => Nat.lt_succ_of_lt (g.ids w hw), empty := g.empty,
               front := ?_, closedEmpty := g.closedEmpty }
      · -- The permits taken out of the free count are now held.
        have := g.conserve
        simp only [total]
        rcases hfresh.2 with h0 | ⟨_, hle⟩
        · subst h0; simp only [Nat.sub_zero]; omega
        · omega
      · -- A waiter at the front keeps its facts, as above.
        intro w rest hq
        have := g.front w rest hq
        rcases hfresh.2 with h0 | ⟨hq0, _⟩
        · subst h0; simp only [Nat.sub_zero]; exact this
        · rw [hq0] at hq; simp at hq
    · -- Refused: only the arrival number moves on.
      exact { conserve := g.conserve, sorted := g.sorted,
              ids := fun w hw => Nat.lt_succ_of_lt (g.ids w hw), empty := g.empty,
              front := g.front, closedEmpty := g.closedEmpty }
  | release id =>
    -- Unfold the step.
    simp only [step]
    -- Was there such a holder?
    split
    -- No: nothing changes.
    · exact g
    · -- Yes: its permits go back to the free count, and the pass runs.
      next n rest hh =>
      apply pass_good
      · -- What the holder held is now free.
        have := g.conserve
        have := takeHolder_total hh
        simp only
        omega
      -- The line did not change.
      · exact g.sorted
      · exact g.ids
      · exact g.closedEmpty
  | cancel id =>
    -- Unfold the step.
    simp only [step]
    -- Was there such a waiter?
    split
    -- No: nothing changes.
    · exact g
    · -- Yes: it leaves the line, and the pass runs.
      next q hq =>
      have hsub := dropWaiter_sublist hq
      apply pass_good
      -- No permit moved.
      · exact g.conserve
      -- A line that lost a waiter is still ordered ...
      · exact g.sorted.sublist hsub
      -- ... and still numbered.
      · intro w hw; exact g.ids w (mem_of_sublist hsub hw)
      · -- A closed semaphore had nobody in line, so there was nobody to drop.
        intro h
        simp only at h
        have h0 := g.closedEmpty h
        rw [h0] at hq
        simp [dropWaiter] at hq
  | close =>
    -- Unfold the step: closed, nobody in line, the set-aside back in the free count.
    simp only [step]
    exact { conserve := by have := g.conserve; simp only; omega,
            sorted := List.Pairwise.nil, ids := by simp, empty := fun _ => rfl,
            front := by simp, closedEmpty := fun _ => rfl }

/-- No step changes the size of the semaphore: every branch of `step` keeps `cap` (each `split`
looks at one branch, and `rfl` sees that `cap` is the same). -/
theorem step_cap (s : Sem) (op : Op) : (step s op).next.cap = s.cap := by
  cases op <;> simp only [step, passStep, pass] <;> repeat (first | split | rfl)

/-- A good state stays good whatever operations follow. -/
theorem run_good (s : Sem) (ops : List Op) (g : Good s) : Good (run s ops) := by
  -- By induction on the operations.
  induction ops generalizing s with
  -- No operation: the state is the one we started from.
  | nil => exact g
  -- One step keeps it good, and the rest by the fact for the rest.
  | cons op ops ih => exact ih _ (step_good s op g)

/-- No sequence of operations changes the size of the semaphore. -/
theorem run_cap (s : Sem) (ops : List Op) : (run s ops).cap = s.cap := by
  -- By induction on the operations.
  induction ops generalizing s with
  | nil => rfl
  -- The rest keeps the size of the state after the first step, which kept it too.
  | cons op ops ih => simp only [run]; rw [ih, step_cap]

end Nuclei.FairSemaphore

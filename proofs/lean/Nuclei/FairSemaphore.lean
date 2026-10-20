/-!
# The bounded channel's fair semaphore, one operation at a time

This file checks the bookkeeping of `FairSemaphore` (`src/sync/fair_semaphore.rs`), the
first-come, first-served semaphore that counts the slots of nuclei's bounded channel.

## The story in plain words

A channel with 3 slots starts with 3 free permits. A sender takes a permit before it puts a
value in; the receiver gives a permit back for every value it takes out. When there are not
enough free permits, a request waits in line. The line is served strictly in order: the free
permits are set aside for the request at the front until it has all it asked for, and nobody
behind it gets anything first, however little they ask for.

A worked example, with 3 permits:

1. Request 0 asks for 3 and gets them at once: free 0, held 3.
2. Request 1 asks for 2 and waits; request 2 asks for 1 and waits behind it.
3. Request 0 gives its 3 back. The front (request 1) needs 2: 2 are set aside and request 1
   is granted. The third permit goes to request 2, which is granted too. Free 0.
4. Request 3 asks for 3 and waits. Request 1 gives 2 back: both are set aside for request 3
   (it still lacks 1), and free stays 0, so a request 4 for 1 permit waits behind request 3
   even though, a moment ago, 2 permits came back.
5. Request 3 is dropped while it waits. Its 2 set-aside permits are not lost: they stay set
   aside and go to request 4, the next in line, which is granted (it needs 1); the other one
   goes back to the free count.

This file proves, for every sequence of operations (acquire, try-acquire, release, cancel,
close) from a fresh semaphore:

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

## What the model leaves out

It is one thread doing one whole operation at a time. In the code, a release, an arrival, a
withdrawal and a pass are separate atomic steps that other threads can interleave; the TLA+
model in `proofs/tla/mpsc` checks those interleavings. Here a pass runs at once after every
operation, as it does in the code once every concurrent step has landed. The values sent
through the permits, the message queue and the wakers are not in this model either.
-/

namespace Nuclei.FairSemaphore

/-! ## The state -/

/-- A request waiting in line: its arrival number and how many permits it asks for.
Example: `⟨4, 1⟩` is the fifth request, asking for one permit. -/
structure Waiter where
  /-- The arrival number: requests are numbered 0, 1, 2, ... in the order they begin. -/
  id : Nat
  /-- How many permits it asks for (the `n` of `reserve_many(n)`). -/
  need : Nat
deriving DecidableEq

/-- Everything the semaphore knows between two operations. -/
structure Sem where
  /-- The permits it was made with: the channel's `buffer`. It never changes. -/
  cap : Nat
  /-- Permits free in the state word: what `capacity()` reports. -/
  free : Nat
  /-- Permits taken out of the word for the front waiter and not yet granted. -/
  setAside : Nat
  /-- The waiters, oldest first. -/
  queue : List Waiter
  /-- Every request that has its permits: (arrival number, permits held). -/
  holding : List (Nat × Nat)
  /-- Whether the receiver closed the channel. -/
  closed : Bool
  /-- The arrival number the next request gets. -/
  nextId : Nat

/-- A fresh semaphore of `cap` permits: all free, nobody waiting, nothing held.
Example: `fresh 3` has `free = 3`. -/
def fresh (cap : Nat) : Sem :=
  { cap := cap, free := cap, setAside := 0, queue := [], holding := [], closed := false,
    nextId := 0 }

/-- The permits held by every entry of a holding list.
Example: `total [(0, 2), (5, 1)] = 3`. -/
def total : List (Nat × Nat) → Nat
  | [] => 0
  | (_, n) :: rest => n + total rest

/-- The permits a list of waiters asks for, together.
Example: the waiters `⟨1, 2⟩` and `⟨2, 1⟩` ask for 3. -/
def needs : List Waiter → Nat
  | [] => 0
  | w :: rest => w.need + needs rest

/-! ## The pass -/

/-- What a pass leaves behind. -/
structure Pass where
  /-- The free count after the pass. -/
  free : Nat
  /-- The set-aside after the pass. -/
  setAside : Nat
  /-- The waiters it granted, in the order it granted them. -/
  granted : List Waiter
  /-- The waiters still in line. -/
  rest : List Waiter

/-- The pass (`FairSemaphore::pass`): move free permits to the set-aside for the front
waiter, at most what it still lacks; grant it once its whole request is set aside, and go on
with the next; stop at a front that is not covered. With nobody left, the set-aside goes back
to the free count.
Example: free 1, set-aside 2, line `[⟨3, 3⟩, ⟨4, 1⟩]`: the front takes the free permit, has
3, is granted, and the pass goes on; the next front needs 1, nothing is left, so the pass
stops with free 0, set-aside 0, granted `[⟨3, 3⟩]`, line `[⟨4, 1⟩]`. -/
def serve (free setAside : Nat) : List Waiter → Pass
  | [] => { free := free + setAside, setAside := 0, granted := [], rest := [] }
  | w :: rest =>
    -- `take_up_to`: what moves from the word to the set-aside.
    let t := min free (w.need - setAside)
    if w.need ≤ setAside + t then
      -- The whole request is set aside: grant it, keep what is left over, and go on.
      let p := serve (free - t) (setAside + t - w.need) rest
      { p with granted := w :: p.granted }
    else
      -- Not covered: the front keeps its share and waits.
      { free := free - t, setAside := setAside + t, granted := [], rest := w :: rest }

/-- Runs the pass on a state: the waiters it grants now hold their permits. -/
def pass (s : Sem) : Sem × List Waiter :=
  let p := serve s.free s.setAside s.queue
  ({ s with free := p.free, setAside := p.setAside, queue := p.rest,
            holding := p.granted.map (fun w => (w.id, w.need)) ++ s.holding },
   p.granted)

/-! ## The operations -/

/-- One operation on the semaphore. -/
inductive Op where
  /-- A new request for `need` permits that waits if it must (`reserve_many(need)`). -/
  | acquire (need : Nat)
  /-- A new request for `need` permits that never waits (`try_reserve_many(need)`). -/
  | tryAcquire (need : Nat)
  /-- The holder with this arrival number gives all its permits back. -/
  | release (id : Nat)
  /-- The waiting request with this arrival number is dropped. -/
  | cancel (id : Nat)
  /-- The receiver closes the channel. -/
  | close

/-- What one step did: the state after it, the waiters it granted from the line, and the
request it granted without the line, if any. -/
structure Step where
  /-- The state after the step. -/
  next : Sem
  /-- Waiters granted from the line, in order. -/
  granted : List Waiter
  /-- A request granted at once, without waiting. -/
  fresh : Option Waiter

/-- Removes the first holder with arrival number `id`, returning its permits and the rest.
Example: `takeHolder 5 [(0, 2), (5, 1)] = some (1, [(0, 2)])`. -/
def takeHolder (id : Nat) : List (Nat × Nat) → Option (Nat × List (Nat × Nat))
  | [] => none
  | (j, n) :: rest =>
    if j = id then some (n, rest)
    else (takeHolder id rest).map (fun (m, r) => (m, (j, n) :: r))

/-- Removes the first waiter with arrival number `id` from the line.
Example: `dropWaiter 3 [⟨3, 2⟩, ⟨4, 1⟩] = some [⟨4, 1⟩]`. -/
def dropWaiter (id : Nat) : List Waiter → Option (List Waiter)
  | [] => none
  | w :: rest => if w.id = id then some rest else (dropWaiter id rest).map (w :: ·)

/-- The state after a step that only ran the pass, as a `Step`. -/
def passStep (s : Sem) : Step :=
  { next := (pass s).1, granted := (pass s).2, fresh := none }

/-- One operation, then the pass, as the code does it.
- An acquire or try-acquire gets the next arrival number. It fails on a closed channel. It
  takes its permits at once when nobody waits and enough are free (or it asks for none);
  otherwise an acquire joins the back of the line and a try-acquire gives up.
- A release gives a holder's permits back; a cancel takes a waiter out of the line.
- The close fails every waiter and puts the set-aside back into the free count. -/
def step (s : Sem) : Op → Step
  | .acquire need =>
    let s1 := { s with nextId := s.nextId + 1 }
    if s.closed then { next := s1, granted := [], fresh := none }
    else if need = 0 ∨ (s.queue = [] ∧ need ≤ s.free) then
      { next := { s1 with free := s.free - need, holding := (s.nextId, need) :: s.holding },
        granted := [], fresh := some ⟨s.nextId, need⟩ }
    else passStep { s1 with queue := s.queue ++ [⟨s.nextId, need⟩] }
  | .tryAcquire need =>
    let s1 := { s with nextId := s.nextId + 1 }
    if ¬s.closed ∧ (need = 0 ∨ (s.queue = [] ∧ need ≤ s.free)) then
      { next := { s1 with free := s.free - need, holding := (s.nextId, need) :: s.holding },
        granted := [], fresh := some ⟨s.nextId, need⟩ }
    else { next := s1, granted := [], fresh := none }
  | .release id =>
    match takeHolder id s.holding with
    | none => { next := s, granted := [], fresh := none }
    | some (n, rest) => passStep { s with free := s.free + n, holding := rest }
  | .cancel id =>
    match dropWaiter id s.queue with
    | none => { next := s, granted := [], fresh := none }
    | some q => passStep { s with queue := q }
  | .close =>
    { next := { s with closed := true, queue := [], free := s.free + s.setAside, setAside := 0 },
      granted := [], fresh := none }

/-- The state after a whole sequence of operations. -/
def run (s : Sem) : List Op → Sem
  | [] => s
  | op :: ops => run (step s op).next ops

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
      -- It took `min free lack` and is still short, so it took every free permit.
      omega

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

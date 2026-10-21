/-!
# The bounded channel's fair semaphore, one operation at a time

This file, with `Invariant.lean` and `Theorems.lean` beside it, checks the bookkeeping of
`FairSemaphore` (`src/sync/fair_semaphore.rs`), the first-come, first-served semaphore that
counts the slots of nuclei's bounded channel. This file defines the model: the state, the
pass, and one whole operation at a time.

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

end Nuclei.FairSemaphore

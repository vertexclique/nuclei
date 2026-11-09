/-!
# The deadline of a blocking receive

This file checks the decision a deadline wait takes after each pending poll (`src/sync/internal.rs`:
`block_on_until`, under every `blocking_recv_deadline` of `src/sync/mpsc` and `src/sync/oneshot.rs`):
`deadline.checked_duration_since(now)?` answers `None` once `now` is past the deadline,
`left.is_zero()` answers `None` at the deadline, and otherwise the thread parks for `left` at most.

## The story in plain words

The wait polls the receive. Pending, it reads the clock: at or past the deadline it answers that
the deadline passed, else it parks until the deadline at most, and every return of the park polls
again first. So a timeout is never answered before the deadline, and a park never outlasts it.
That every return polls first is the TLA+ model's part (`proofs/tla/blocking`, `FinalPoll`).

## What is abstract

Time is a natural number of the clock's units; `Instant` arithmetic does not overflow here.
-/

namespace Nuclei.Deadline

/-- What the wait does after a pending poll at time `now`: `none` answers the timeout, `some left`
parks for `left` at most. -/
def next (now deadline : Nat) : Option Nat :=
  if deadline ≤ now then none else some (deadline - now)

/-- A timeout is answered only at or past the deadline. -/
theorem timeout_not_early {now deadline : Nat} (h : next now deadline = none) :
    deadline ≤ now := by
  unfold next at h
  split at h
  · assumption
  · cases h

/-- A park lasts more than nothing and ends at the deadline at the latest. -/
theorem park_within {now deadline left : Nat} (h : next now deadline = some left) :
    0 < left ∧ now + left = deadline := by
  unfold next at h
  split at h
  · cases h
  · injection h with h
    omega

/-- Before the deadline the wait parks, it does not answer the timeout. -/
theorem parks_before {now deadline : Nat} (h : now < deadline) :
    next now deadline = some (deadline - now) := by
  unfold next
  split
  · exfalso
    omega
  · rfl

end Nuclei.Deadline

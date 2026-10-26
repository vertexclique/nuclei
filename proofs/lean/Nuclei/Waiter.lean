/-!
# The waiter node's state word

This file checks the bit arithmetic of a waiter node's state word (`src/sync/waiter/node.rs`), with
the word's own constants and the expressions of `claim`, `claimable`, the register swap of `park`,
its arming swap and the end of a take, over every value the twelve bits can hold.

## The story in plain words

A claim makes the current slot the taken one, sets the taking bit and the wake list's reference,
and clears the armed bit; it leaves the phase, the marks and the other references alone. A node
just claimed is not claimable, so one wake list at a time takes from it. The register swap of a
future that parks clears the armed bit and sets the registering bit, so a claim never meets a
registering future, and while a take is under way it picks the slot the take is not reading. It
leaves the phase and the nudge alone, so the arming swap after it reads them. A take that claims
again keeps the taking bit and the wake reference; one that ends clears both and nothing else.

A worked example, computed below: a node armed in slot 0 with the queue's and the future's
references is claimed, then a take under way finds the future registering in slot 1.

## What the model leaves out

Who does what when (the interleaving of the future, the drain passes, the wake lists and the
pool) is the TLA+ model's part, in `proofs/tla/waiter/`, which takes the bits as separate flags;
this file shows the word's encoding gives those flags what the model assumes.
-/

namespace Nuclei.Waiter

def PHASE : Nat := 0b11
def GRANTED : Nat := 1
def REGISTERING : Nat := 1 <<< 2
def ARMED : Nat := 1 <<< 3
def TAKING : Nat := 1 <<< 4
def CURRENT : Nat := 1 <<< 5
def TAKEN : Nat := 1 <<< 6
def NUDGED : Nat := 1 <<< 7
def OWED : Nat := 1 <<< 8
def QUEUE_REF : Nat := 1 <<< 9
def FUTURE_REF : Nat := 1 <<< 10
def WAKE_REF : Nat := 1 <<< 11

/-- Every bit of the word. -/
def ALL : Nat := (1 <<< 12) - 1

/-- `s & !m`, within the word. -/
def clear (s m : Nat) : Nat := s &&& (ALL ^^^ m)

def has (s b : Nat) : Bool := s &&& b != 0

/-- `claimable`: armed, and nobody registering or taking. -/
def claimable (s : Nat) : Bool := s &&& (ARMED ||| REGISTERING ||| TAKING) == ARMED

/-- `claim`: the current slot becomes the taken one, for a wake list. -/
def claim (s : Nat) : Nat :=
  clear s (ARMED ||| TAKEN) ||| (if has s CURRENT then TAKEN else 0) ||| TAKING ||| WAKE_REF

/-- The register swap of `park`: the slot a take under way is not reading, else the current. -/
def register (s : Nat) : Nat :=
  let current := if has s TAKING then !has s TAKEN else has s CURRENT
  clear s (ARMED ||| CURRENT) ||| REGISTERING ||| (if current then CURRENT else 0)

/-- The arming swap of `park`. -/
def arm (s : Nat) : Nat := clear s (REGISTERING ||| NUDGED) ||| ARMED

/-- `take_claimed`'s test for a waker armed during the take, and the end of a take. -/
def again (s : Nat) : Bool :=
  s &&& (ARMED ||| REGISTERING) == ARMED && (has s NUDGED || s &&& PHASE == GRANTED)

def takeEnd (s : Nat) : Nat := clear s (TAKING ||| WAKE_REF)

/-- Whether `p` holds of every value of the word's twelve bits. -/
def every (p : Nat → Bool) : Bool := (List.range 4096).all p

theorem of_every {p : Nat → Bool} (h : every p = true) : ∀ s, s < 4096 → p s = true := by
  intro s hs
  simp only [every, List.all_eq_true, List.mem_range] at h
  exact h s hs

/-- The bits a claim and the register swap leave alone. -/
def KEPT : Nat := PHASE ||| NUDGED ||| OWED ||| QUEUE_REF ||| FUTURE_REF

/-- A claim takes the current slot, sets the taking bit and the wake reference, disarms. -/
theorem claim_takes_the_current_slot : ∀ s, s < 4096 →
    (has (claim s) TAKEN == has s CURRENT && has (claim s) TAKING && has (claim s) WAKE_REF &&
      !has (claim s) ARMED) = true :=
  of_every (by decide +kernel)

/-- A claim leaves the phase, the marks and the other references alone. -/
theorem claim_keeps_the_rest : ∀ s, s < 4096 → (claim s &&& KEPT == s &&& KEPT) = true :=
  of_every (by decide +kernel)

/-- A node just claimed is not claimable: one wake list takes from it at a time. -/
theorem one_claim_at_a_time : ∀ s, s < 4096 → (!claimable (claim s)) = true :=
  of_every (by decide +kernel)

/-- The register swap disarms and sets the registering bit: no claim meets it. -/
theorem register_disarms : ∀ s, s < 4096 →
    (!has (register s) ARMED && has (register s) REGISTERING && !claimable (register s)) = true :=
  of_every (by decide +kernel)

/-- While a take is under way, the future registers in the slot the take is not reading. -/
theorem register_avoids_the_taken_slot : ∀ s, s < 4096 →
    (!has s TAKING || has (register s) CURRENT != has s TAKEN) = true :=
  of_every (by decide +kernel)

/-- The register swap leaves the phase and the marks for the arming swap to read. -/
theorem register_keeps_the_marks : ∀ s, s < 4096 → (register s &&& KEPT == s &&& KEPT) = true :=
  of_every (by decide +kernel)

/-- The arming swap arms, ends the registration and takes the nudge, and keeps the slots. -/
theorem arm_arms : ∀ s, s < 4096 →
    (has (arm s) ARMED && !has (arm s) REGISTERING && !has (arm s) NUDGED &&
      has (arm s) CURRENT == has s CURRENT && has (arm s) TAKEN == has s TAKEN) = true :=
  of_every (by decide +kernel)

/-- A take that claims again keeps the taking bit and the wake reference. -/
theorem claiming_again_keeps_the_take : ∀ s, s < 4096 →
    (!again s || has (claim (clear s TAKING)) TAKING && has (claim (clear s TAKING)) WAKE_REF) =
      true :=
  of_every (by decide +kernel)

/-- A take that ends clears the taking bit and the wake reference, and nothing else. -/
theorem a_take_ends_cleanly : ∀ s, s < 4096 →
    (takeEnd s ||| (s &&& (TAKING ||| WAKE_REF)) == s && !has (takeEnd s) TAKING &&
      !has (takeEnd s) WAKE_REF) = true :=
  of_every (by decide +kernel)

-- The worked example: armed in slot 0 with the queue's and the future's references, claimed,
-- then the future registers while the take is under way.
example : claim (ARMED ||| QUEUE_REF ||| FUTURE_REF) =
    TAKING ||| QUEUE_REF ||| FUTURE_REF ||| WAKE_REF := by decide +kernel
example : has (register (claim (ARMED ||| QUEUE_REF ||| FUTURE_REF))) CURRENT = true := by decide +kernel

end Nuclei.Waiter

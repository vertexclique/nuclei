/-!
# The reader-writer lock's state word

This file checks the rules of the reader-writer lock's state word (`src/sync/raw_rwlock.rs`): the
readers inside, a writer bit, an upgradable bit and an upgrading bit, and what each kind of entry,
each release, a downgrade and an upgrade's finish do to them.

## The story in plain words

The lock is in a good state when a writer, if any, is alone inside: no reader and no upgradable
read beside it, and upgrading only ever goes with the upgradable read it belongs to. A read enters
while there is no writer and no upgrade under way; a write while nobody at all is inside; an
upgradable read while there is no writer and no other upgradable read. Each of those entries, and
each release, takes a good state to a good one, so a writer never shares the lock and there is
never more than one upgradable read. A downgrade turns the writer into one reader. An upgrade
raises the upgrading bit, which holds new readers back, and finishes only with no reader inside,
turning the upgradable read into the writer: a good state again.

A worked example, computed below: two readers and the upgradable read, upgrading: a read is held
back, a write is held back, and once both readers left the upgrade finishes into a lone writer.

## What the model leaves out

The waiting (queues, nudges, the handoff, the upgrade's slot and the pass that grants it) is the
TLA+ model's part, in `proofs/tla/rwlock/`. The bound on readers (`max_readers`) is left out here:
it only holds a read back, never lets one in.
-/

namespace Nuclei.RwLock

structure S where
  readers : Nat
  writer : Bool
  upgradable : Bool
  upgrading : Bool
  deriving DecidableEq, Repr

/-- A good state: a writer is alone, and upgrading goes with the upgradable read. -/
def Good (s : S) : Prop :=
  (s.writer = true → s.readers = 0 ∧ s.upgradable = false) ∧
  (s.upgrading = true → s.upgradable = true)

inductive Kind | read | write | upg
  deriving DecidableEq

/-- Whether a kind enters a lock in state `s` (`admits`, the reader bound aside). -/
def admits : Kind → S → Bool
  | .read, s => !s.writer && !s.upgrading
  | .write, s => !s.writer && !s.upgradable && s.readers == 0
  | .upg, s => !s.writer && !s.upgradable

/-- The state with a kind entered (`enter`). -/
def enter : Kind → S → S
  | .read, s => { s with readers := s.readers + 1 }
  | .write, s => { s with writer := true }
  | .upg, s => { s with upgradable := true }

/-- An admitted entry keeps a good state good. -/
theorem enter_keeps_good (k : Kind) (s : S) (g : Good s) (a : admits k s = true) :
    Good (enter k s) := by
  obtain ⟨gw, gu⟩ := g
  cases k <;> simp only [admits, Bool.and_eq_true, Bool.not_eq_eq_eq_not, Bool.not_true,
    beq_iff_eq] at a <;> simp only [Good, enter] <;> refine ⟨?_, ?_⟩ <;> intros <;> simp_all

/-- A read's release (by a reader inside) keeps a good state good. -/
theorem read_release_keeps_good (s : S) (g : Good s) :
    Good { s with readers := s.readers - 1 } := by
  obtain ⟨gw, gu⟩ := g
  refine ⟨fun w => ?_, gu⟩
  have h := gw w
  exact ⟨by simp [h.1], h.2⟩

/-- A write's release, and the upgradable read's (which drops the upgrade too), keep it good. -/
theorem write_release_keeps_good (s : S) (g : Good s) : Good { s with writer := false } := by
  obtain ⟨_, gu⟩ := g
  exact ⟨fun h => by simp at h, gu⟩

theorem upgradable_release_keeps_good (s : S) (g : Good s) :
    Good { s with upgradable := false, upgrading := false } := by
  obtain ⟨gw, _⟩ := g
  exact ⟨fun w => ⟨(gw w).1, rfl⟩, fun h => by simp at h⟩

/-- A downgrade (by the writer, alone) turns it into one reader: a good state. -/
theorem downgrade_keeps_good (s : S) (g : Good s) :
    Good { s with writer := false, readers := s.readers + 1 } := by
  obtain ⟨_, gu⟩ := g
  exact ⟨fun h => by simp at h, gu⟩

/-- An upgrade that finishes with no reader inside turns the upgradable read into a lone writer. -/
theorem upgrade_finish_keeps_good (s : S) (noReaders : s.readers = 0) :
    Good { s with upgradable := false, upgrading := false, writer := true } := by
  exact ⟨fun _ => ⟨noReaders, rfl⟩, fun h => by simp at h⟩

/-- While an upgrade is under way no read enters, and while anyone is inside no write does. -/
theorem upgrading_holds_reads_back (s : S) (u : s.upgrading = true) : admits .read s = false := by
  simp [admits, u]

theorem a_write_needs_the_lock_empty (s : S) (inside : s.readers > 0 ∨ s.upgradable = true) :
    admits .write s = false := by
  rcases inside with h | h
  · simp [admits]
    intros
    omega
  · simp [admits, h]

-- The worked example: two readers and the upgradable read, upgrading.
example : admits .read ⟨2, false, true, true⟩ = false := by decide
example : admits .write ⟨2, false, true, true⟩ = false := by decide
example : Good ⟨0, true, false, false⟩ := by simp [Good]

end Nuclei.RwLock

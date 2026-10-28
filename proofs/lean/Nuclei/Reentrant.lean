/-!
# The reentrant locks' depths and shares

This file checks, for one owner and at any depth, the accounting the two reentrant locks rest on:
the reentrant mutex (`src/sync/reentrant_mutex.rs`) holds its permit exactly while its depth is
above zero, and the reentrant reader-writer lock (`src/sync/reentrant_rwlock.rs`) holds, for each
owner, exactly the share of the plain lock its read and write depths call for.

## The story in plain words

The mutex: a first lock takes the permit and sets the depth to one; a lock again adds one; an
unlock takes one off, and the last gives the permit back. So whatever the owner's locks and
unlocks, it holds the permit exactly while its depth is above zero, and the depth is the guards
it holds.

The reader-writer lock: an owner that writes holds the plain lock's write, one that reads without
writing holds a read share, one that does neither holds nothing. A read when the owner already
reads or writes takes no share of its own; a first read takes a read share; a write again adds a
level; a write when the owner reads without writing is refused, since it would wait for its own
read; a first write takes the write. The last read gives its share back unless the owner writes;
the last write turns into a read share when the owner still reads (a downgrade), and is given back
otherwise. Each of those steps keeps the share the one the depths call for, so it is that share
after any sequence of steps, however deep.

A worked example, computed below: write, read, let the write go, let the read go: the write, the
write still, a read share, nothing.

## What the model leaves out

Other owners, the waits for the permit or the plain lock, the holder id and the records (how an
owner finds that it holds the lock) are the TLA+ model's part, in `proofs/tla/reentrant/`, which
checks them only a few levels deep; this file covers every depth for one owner. An unlock without
a guard to drop cannot happen (a guard's drop is the unlock), so it is a step that changes
nothing here.
-/

namespace Nuclei.Reentrant

inductive Op | lock | unlock
  deriving DecidableEq

/-- One owner of the mutex: its depth and whether it holds the permit. -/
structure M where
  depth : Nat
  permit : Bool
  deriving DecidableEq, Repr

/-- What a lock (`reenter`, else the permit and `enter`) and an unlock do to the owner. -/
def stepM : M → Op → M
  | ⟨0, _⟩, .lock => ⟨1, true⟩
  | ⟨d + 1, p⟩, .lock => ⟨d + 2, p⟩
  | ⟨1, _⟩, .unlock => ⟨0, false⟩
  | ⟨d + 2, p⟩, .unlock => ⟨d + 1, p⟩
  | s@⟨0, _⟩, .unlock => s

def runM (s : M) (ops : List Op) : M := ops.foldl stepM s

/-- The owner holds the permit exactly while its depth is above zero. -/
def GoodM (s : M) : Prop := s.permit = decide (0 < s.depth)

theorem stepM_keeps_good (s : M) (op : Op) (g : GoodM s) : GoodM (stepM s op) := by
  obtain ⟨d, p⟩ := s
  unfold GoodM at *
  cases op <;> rcases d with _ | _ | d <;> simp_all [stepM]

/-- Whatever its locks and unlocks, the owner holds the permit exactly while its depth is up. -/
theorem the_permit_follows_the_depth (ops : List Op) : GoodM (runM ⟨0, false⟩ ops) := by
  unfold runM
  suffices h : ∀ s, GoodM s → GoodM (ops.foldl stepM s) from h _ (by simp [GoodM])
  induction ops with
  | nil => exact fun _ g => g
  | cons op rest ih => exact fun s g => ih _ (stepM_keeps_good s op g)

/-- A lock adds one level, and an unlock that drops a guard takes one off. -/
theorem lock_adds_one (s : M) : (stepM s .lock).depth = s.depth + 1 := by
  obtain ⟨d, p⟩ := s
  rcases d with _ | d <;> simp [stepM]

theorem unlock_takes_one (s : M) : (stepM s .unlock).depth = s.depth - 1 := by
  obtain ⟨d, p⟩ := s
  rcases d with _ | _ | d <;> simp [stepM]

inductive Share | none | read | write
  deriving DecidableEq, Repr

/-- One owner of the reader-writer lock: its read and write depths and its share. -/
structure O where
  reads : Nat
  writes : Nat
  share : Share
  deriving DecidableEq, Repr

/-- The share the depths call for. -/
def shareOf (reads writes : Nat) : Share :=
  if 0 < writes then .write else if 0 < reads then .read else .none

inductive Step | read | write | readUnlock | writeUnlock
  deriving DecidableEq

/-- What each step does to the owner; `none` for a refused write or an unlock with no guard. -/
def step (s : O) : Step → Option O
  | .read =>
    if 0 < s.reads ∨ 0 < s.writes then some { s with reads := s.reads + 1 }
    else some { s with reads := 1, share := .read }
  | .write =>
    if 0 < s.writes then some { s with writes := s.writes + 1 }
    else if 0 < s.reads then none
    else some { s with writes := 1, share := .write }
  | .readUnlock =>
    if s.reads = 0 then none
    else if s.reads = 1 then
      some { s with reads := 0, share := if 0 < s.writes then s.share else .none }
    else some { s with reads := s.reads - 1 }
  | .writeUnlock =>
    if s.writes = 0 then none
    else if s.writes = 1 then
      some { s with writes := 0, share := if 0 < s.reads then .read else .none }
    else some { s with writes := s.writes - 1 }

/-- A run of steps; a refused write (`ReadHeldByOwner`) leaves the owner as it was. -/
def run (s : O) (steps : List Step) : O := steps.foldl (fun s k => (step s k).getD s) s

def Good (s : O) : Prop := s.share = shareOf s.reads s.writes

theorem step_keeps_good (s s' : O) (k : Step) (g : Good s) (h : step s k = some s') : Good s' := by
  obtain ⟨r, w, sh⟩ := s
  unfold Good shareOf at *
  cases k <;> unfold step at h <;> dsimp only at h <;> repeat' split at h
  all_goals first
    | contradiction
    | (injection h with h; subst h; dsimp only; repeat' split)
  all_goals simp_all <;> omega

/-- After any steps, however deep, the owner holds exactly the share its depths call for. -/
theorem the_share_follows_the_depths (steps : List Step) : Good (run ⟨0, 0, .none⟩ steps) := by
  unfold run
  suffices h : ∀ s, Good s → Good (steps.foldl (fun s k => (step s k).getD s) s) from
    h _ (by simp [Good, shareOf])
  induction steps with
  | nil => exact fun _ g => g
  | cons k rest ih =>
    intro s g
    apply ih
    show Good ((step s k).getD s)
    cases h : step s k with
    | none => exact g
    | some s' => exact step_keeps_good s s' k g h

/-- A write is refused exactly when the owner reads without writing. -/
theorem refused_iff (s : O) : step s .write = none ↔ s.writes = 0 ∧ 0 < s.reads := by
  by_cases hw : 0 < s.writes <;> by_cases hr : 0 < s.reads <;> simp [step, hw, hr] <;> omega

-- The worked example: write, read, let the write go, let the read go.
example : run ⟨0, 0, .none⟩ [.write] = ⟨0, 1, .write⟩ := by decide
example : run ⟨0, 0, .none⟩ [.write, .read] = ⟨1, 1, .write⟩ := by decide
example : run ⟨0, 0, .none⟩ [.write, .read, .writeUnlock] = ⟨1, 0, .read⟩ := by decide
example : run ⟨0, 0, .none⟩ [.write, .read, .writeUnlock, .readUnlock] = ⟨0, 0, .none⟩ := by
  decide

end Nuclei.Reentrant

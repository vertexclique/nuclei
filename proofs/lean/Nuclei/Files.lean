/-!
# A ring's file slots, one operation at a time

This file checks the bookkeeping of a ring's file slots (`src/syscore/linux/iouring/files.rs`,
and `ShardRing::take_out` in `src/rt/shard/ring.rs`): which slots a source holds, how many
submitted entries still name each one, whether the ring's table holds a file there, and when a
slot goes back to be handed out again.

## The story in plain words

Every operation a source puts on its shard's ring names the source's file by a slot, never by
a number the ring made: a slot of the kernel's fixed-file table, which holds the file, or, past
a full table, a slot naming the source's own descriptor (the ring makes no copy of it, so a
process with no descriptor to spare registers its sources all the same). A source takes a slot
when it first uses the ring and releases it when it goes. As it goes its file leaves the table
at once, whatever entries still name the slot, so the kernel closes the file as soon as the
requests that hold it let go. Each entry that names the slot counts on it from its submission
to its completion. A slot goes back to be handed out only when no source holds it and no entry
names it. So an entry still on its way, queued or in the kernel, never reaches the file of the
next source that takes the slot.

A worked example, computed below (`example_reuse_waits`): a table of one slot.

1. Source A takes slot 0, whose table now holds A's file, and submits a read: one entry names
   slot 0.
2. A goes and releases slot 0. Its file leaves the table now, but the read is still out, so the
   slot is not free.
3. Source B asks for a slot of the table: none is free, so it gets none (it runs on its own
   descriptor, past the table).
4. The read completes: no entry names slot 0, nobody holds it, it is free.
5. Now a source may take slot 0.

## What the model leaves out

It is one thread doing one whole operation at a time, as the ring's own thread does: only
that thread ever touches these slots. The free lists are a predicate here, so a slot is
either free or not; the code keeps lists, and the theorem `no_double_free` is what keeps a slot
from going on its list twice. An entry is counted on a slot only while a source holds it,
which the code's callers ensure: a source submits on the slot of its registration, and its
registration goes (both sides held) before the slot is released. The release stands for the
code's take-out: the entries naming the slot reached the kernel first, which
`proofs/tla/ring/RingClose.tla` checks with the lease that keeps the source's object open until
then. The kernel's table is `fileIn`; a slot naming a source's own descriptor holds nothing of
the ring's.
-/

namespace Nuclei.Files

/-! ## The state -/

/-- What one slot is doing (`Slot` in the code). -/
structure Slot where
  /-- A source holds it. -/
  held : Bool
  /-- Entries that name it and have not completed. -/
  entries : Nat
  /-- The kernel's table holds the source's file in this slot. -/
  fileIn : Bool

/-- A ring's slots: what each is doing, which are free, and how many the kernel's table has
(slots `0 .. fixed - 1`; the ones from `fixed` on hold copies). -/
structure Files where
  slot : Nat → Slot
  isFree : Nat → Bool
  fixed : Nat

/-- `f` with `s` mapped to `v`. -/
def upd {α : Type} (f : Nat → α) (s : Nat) (v : α) : Nat → α :=
  fun t => if t = s then v else f t

@[simp] theorem upd_same {α : Type} (f : Nat → α) (s : Nat) (v : α) : upd f s v s = v := by
  simp [upd]

@[simp] theorem upd_other {α : Type} (f : Nat → α) (s t : Nat) (v : α) (h : t ≠ s) :
    upd f s v t = f t := by
  simp [upd, h]

/-- A table of `fixed` slots, all free; every slot past it is free too (the code makes it
when first needed, free). -/
def init (fixed : Nat) : Files where
  slot := fun _ => ⟨false, 0, false⟩
  isFree := fun _ => true
  fixed := fixed

/-! ## The operations -/

/-- What the ring's thread does to the slots. -/
inductive Op where
  /-- A source takes slot `s` of the kernel's table, which holds its file (`take`). -/
  | take (s : Nat)
  /-- A source takes slot `s` past the table, naming its own descriptor (`take_own`). -/
  | takeOwn (s : Nat)
  /-- An entry naming `s` is submitted (`named`). -/
  | named (s : Nat)
  /-- An entry naming `s` completed (`done`). -/
  | done (s : Nat)
  /-- The source holding `s` went: its file leaves the table, and the slot is released
  (`take_out`, `release`). -/
  | release (s : Nat)

/-- Frees `s`: it can be handed out again (`free_slot`). -/
def freeSlot (f : Files) (s : Nat) : Files :=
  { f with isFree := upd f.isFree s true, slot := upd f.slot s { f.slot s with fileIn := false } }

/-- One operation. One that does not apply (a take of a slot that is not free, an entry
counted on a slot nobody holds, a completion on a slot no entry names, a release of a slot
nobody holds) changes nothing. -/
def step (f : Files) : Op → Files
  | .take s =>
    if f.isFree s = true ∧ s < f.fixed then
      { f with
        isFree := upd f.isFree s false
        slot := upd f.slot s { f.slot s with held := true, fileIn := true } }
    else f
  | .takeOwn s =>
    if f.isFree s = true ∧ f.fixed ≤ s then
      { f with isFree := upd f.isFree s false, slot := upd f.slot s { f.slot s with held := true } }
    else f
  | .named s =>
    if (f.slot s).held = true then
      { f with slot := upd f.slot s { f.slot s with entries := (f.slot s).entries + 1 } }
    else f
  | .done s =>
    if 0 < (f.slot s).entries then
      let f' := { f with slot := upd f.slot s { f.slot s with entries := (f.slot s).entries - 1 } }
      if (f.slot s).held = false ∧ (f.slot s).entries - 1 = 0 then freeSlot f' s else f'
    else f
  | .release s =>
    if (f.slot s).held = true then
      let f' := { f with slot := upd f.slot s { f.slot s with held := false, fileIn := false } }
      if (f.slot s).entries = 0 then freeSlot f' s else f'
    else f

/-- Operations in order. -/
def run (f : Files) : List Op → Files
  | [] => f
  | op :: ops => run (step f op) ops

/-! ## The invariant -/

/-- A free slot is held by nobody, named by no entry, and holds no file. -/
def Inv (f : Files) : Prop :=
  ∀ s, f.isFree s = true → (f.slot s).held = false ∧ (f.slot s).entries = 0 ∧
    (f.slot s).fileIn = false

theorem inv_init (fixed : Nat) : Inv (init fixed) := by
  intro s _
  simp [init]

theorem inv_freeSlot (f : Files) (s : Nat) (h : Inv f) (held : (f.slot s).held = false)
    (unnamed : (f.slot s).entries = 0) : Inv (freeSlot f s) := by
  intro t ht
  by_cases e : t = s
  · subst e
    simp [freeSlot, held, unnamed]
  · simp [freeSlot, e] at ht ⊢
    exact h t ht

/-- Every operation keeps the invariant. -/
theorem inv_step (f : Files) (op : Op) (h : Inv f) : Inv (step f op) := by
  cases op with
  | take s =>
    simp only [step]
    split
    · intro t ht
      by_cases e : t = s
      · subst e; simp at ht
      · simp [e] at ht ⊢; exact h t ht
    · exact h
  | takeOwn s =>
    simp only [step]
    split
    · intro t ht
      by_cases e : t = s
      · subst e; simp at ht
      · simp [e] at ht ⊢; exact h t ht
    · exact h
  | named s =>
    simp only [step]
    split
    · rename_i hs
      intro t ht
      by_cases e : t = s
      · subst e
        have := (h t ht).1
        simp [hs] at this
      · simp [e] at ht ⊢; exact h t ht
    · exact h
  | done s =>
    simp only [step]
    split
    · rename_i pos
      have notFree : f.isFree s = false := by
        cases hf : f.isFree s
        · rfl
        · have := (h s hf).2.1; omega
      split
      · rename_i cond
        apply inv_freeSlot
        · intro t ht
          by_cases e : t = s
          · subst e; simp [notFree] at ht
          · simp [e] at ht ⊢; exact h t ht
        · simp [cond.1]
        · simp [cond.2]
      · intro t ht
        by_cases e : t = s
        · subst e; simp [notFree] at ht
        · simp [e] at ht ⊢; exact h t ht
    · exact h
  | release s =>
    simp only [step]
    split
    · rename_i hs
      have notFree : f.isFree s = false := by
        cases hf : f.isFree s
        · rfl
        · have := (h s hf).1; simp [hs] at this
      split
      · rename_i unnamed
        apply inv_freeSlot
        · intro t ht
          by_cases e : t = s
          · subst e; simp [notFree] at ht
          · simp [e] at ht ⊢; exact h t ht
        · simp
        · simp [unnamed]
      · intro t ht
        by_cases e : t = s
        · subst e; simp [notFree] at ht
        · simp [e] at ht ⊢; exact h t ht
    · exact h

/-- Any sequence of operations from a fresh table keeps the invariant. -/
theorem inv_run (fixed : Nat) (ops : List Op) : Inv (run (init fixed) ops) := by
  suffices ∀ f, Inv f → Inv (run f ops) from this _ (inv_init fixed)
  induction ops with
  | nil => intro f h; exact h
  | cons op ops ih => intro f h; exact ih _ (inv_step f op h)

/-! ## The theorems -/

/-- A slot a source takes, of the table or past it, has no entry naming it: an entry still on
its way never reaches the file of the source that takes the slot next. -/
theorem taken_slot_is_unnamed (f : Files) (h : Inv f) (s : Nat)
    (took : step f (.take s) ≠ f ∨ step f (.takeOwn s) ≠ f) : (f.slot s).entries = 0 := by
  have free : f.isFree s = true := by
    rcases took with t | t
    · simp only [step] at t; split at t
      · rename_i c; exact c.1
      · exact absurd rfl t
    · simp only [step] at t; split at t
      · rename_i c; exact c.1
      · exact absurd rfl t
  exact (h s free).2.1

/-- A slot that is free is never freed again: a completion or a release finds nothing to do
on it, so it never goes on its list twice. -/
theorem no_double_free (f : Files) (h : Inv f) (s : Nat) (free : f.isFree s = true) :
    step f (.done s) = f ∧ step f (.release s) = f := by
  have ⟨notHeld, unnamed, _⟩ := h s free
  constructor
  · simp [step, unnamed]
  · simp [step, notHeld]

/-- A slot an entry still names is never free, so never handed to another source: a slot
whose source went stays reserved until the last entry naming it completed. -/
theorem named_slot_is_not_free (f : Files) (h : Inv f) (s : Nat)
    (named : 0 < (f.slot s).entries) : f.isFree s = false := by
  cases hf : f.isFree s
  · rfl
  · have := (h s hf).2.1; omega

/-- A source that goes takes its file out of the table at once, whatever entries still name
the slot: the kernel then closes the file as soon as the requests that hold it let go, and the
ring holds nothing of it. -/
theorem release_takes_the_file_out (f : Files) (s : Nat) (held : (f.slot s).held = true) :
    ((step f (.release s)).slot s).fileIn = false := by
  by_cases u : (f.slot s).entries = 0
  · simp [step, held, u, freeSlot]
  · simp [step, held, u]

/-! ## The worked example -/

/-- The story's run: a table of one slot; A takes it and submits a read, releases it with the
read out (its file out of the table at once), B's take finds slot 0 not free (nobody holds it
after), the read completes, and the slot is free again, for the next take. -/
theorem example_reuse_waits :
    let afterRelease := run (init 1) [.take 0, .named 0, .release 0]
    afterRelease.isFree 0 = false ∧
    (afterRelease.slot 0).fileIn = false ∧
    ((step afterRelease (.take 0)).slot 0).held = false ∧
    (step afterRelease (.done 0)).isFree 0 = true ∧
    ((step (step afterRelease (.done 0)) (.take 0)).slot 0).held = true := by
  decide

end Nuclei.Files

/-!
# A ring's file slots, one operation at a time

This file checks the bookkeeping of a ring's file slots (`src/syscore/linux/iouring/files.rs`):
which slots a source holds, how many submitted entries still name each one, and when a slot
goes back to be handed out again.

## The story in plain words

Every operation a source puts on its shard's ring names the source's file by a slot, never by
the descriptor's number: a slot of the kernel's fixed-file table, or, past a full table, a slot
holding the ring's own copy of the descriptor. A source takes a slot when it first uses the
ring and releases it when it goes. Each entry that names the slot counts on it from its
submission to its completion. A slot goes back to be handed out, and a copy closes, only when
no source holds it and no entry names it. So an entry still on its way, queued or in the
kernel, never reaches the file of the next source that takes the slot, and a copy's number
is never reused while an entry may still name it.

A worked example, computed below (`example_reuse_waits`): a table of one slot.

1. Source A takes slot 0 and submits a read: one entry names slot 0.
2. A goes and releases slot 0. The read is still out, so the slot is not free.
3. Source B asks for a slot of the table: none is free, so it gets none (it runs on a copy).
4. The read completes: no entry names slot 0, nobody holds it, it is free.
5. Now a source may take slot 0.

## What the model leaves out

It is one thread doing one whole operation at a time, as the ring's own thread does: only
that thread ever touches these slots. The free lists are a predicate here, so a slot is
either free or not; the code keeps lists, and the theorem `no_double_free` is what keeps a slot
from going on its list twice. An entry is counted on a slot only while a source holds it,
which the code's callers ensure: a source submits on the slot of its registration, and its
registration goes (both sides held) before the slot is released. The kernel's table and the
copies' descriptors are not modelled; `copyOpen` stands for a copy's descriptor being open.
-/

namespace Nuclei.Files

/-! ## The state -/

/-- What one slot is doing (`Slot` in the code). -/
structure Slot where
  /-- A source holds it. -/
  held : Bool
  /-- Entries that name it and have not completed. -/
  entries : Nat
  /-- Past the kernel's table: the ring's copy of the source's descriptor is open. -/
  copyOpen : Bool

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
  /-- A source takes slot `s` of the kernel's table (`take`). -/
  | take (s : Nat)
  /-- A source takes slot `s` past the table, holding a copy (`take_copy`). -/
  | takeCopy (s : Nat)
  /-- An entry naming `s` is submitted (`named`). -/
  | named (s : Nat)
  /-- An entry naming `s` completed (`done`). -/
  | done (s : Nat)
  /-- The source holding `s` went (`release`). -/
  | release (s : Nat)

/-- Frees `s`: it can be handed out again, and a copy it held closes (`free_slot`). -/
def freeSlot (f : Files) (s : Nat) : Files :=
  { f with isFree := upd f.isFree s true, slot := upd f.slot s { f.slot s with copyOpen := false } }

/-- One operation. One that does not apply (a take of a slot that is not free, an entry
counted on a slot nobody holds, a completion on a slot no entry names, a release of a slot
nobody holds) changes nothing. -/
def step (f : Files) : Op → Files
  | .take s =>
    if f.isFree s = true ∧ s < f.fixed then
      { f with isFree := upd f.isFree s false, slot := upd f.slot s { f.slot s with held := true } }
    else f
  | .takeCopy s =>
    if f.isFree s = true ∧ f.fixed ≤ s then
      { f with
        isFree := upd f.isFree s false
        slot := upd f.slot s { f.slot s with held := true, copyOpen := true } }
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
      let f' := { f with slot := upd f.slot s { f.slot s with held := false } }
      if (f.slot s).entries = 0 then freeSlot f' s else f'
    else f

/-- Operations in order. -/
def run (f : Files) : List Op → Files
  | [] => f
  | op :: ops => run (step f op) ops

/-! ## The invariant -/

/-- A free slot is held by nobody, named by no entry, and holds no open copy. -/
def Inv (f : Files) : Prop :=
  ∀ s, f.isFree s = true → (f.slot s).held = false ∧ (f.slot s).entries = 0 ∧
    (f.slot s).copyOpen = false

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
  | takeCopy s =>
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
    (took : step f (.take s) ≠ f ∨ step f (.takeCopy s) ≠ f) : (f.slot s).entries = 0 := by
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

/-- A copy closes only when no source holds its slot and no entry names it: its number is
never free for reuse while an entry may still name it. -/
theorem copy_closes_unnamed (f : Files) (h : Inv f) (op : Op) (s : Nat)
    (wasOpen : (f.slot s).copyOpen = true) (closed : ((step f op).slot s).copyOpen = false) :
    ((step f op).slot s).held = false ∧ ((step f op).slot s).entries = 0 := by
  -- The only step that closes a copy is the one that frees its slot.
  have nowFree : (step f op).isFree s = true := by
    cases op with
    | take t =>
      simp only [step] at closed ⊢
      by_cases c : f.isFree t = true ∧ t < f.fixed
      · rw [ite_eq_left c] at closed
        by_cases e : s = t
        · subst e; simp [wasOpen] at closed
        · simp [e, wasOpen] at closed
      · rw [ite_eq_right c] at closed; simp [wasOpen] at closed
    | takeCopy t =>
      simp only [step] at closed ⊢
      by_cases c : f.isFree t = true ∧ f.fixed ≤ t
      · rw [ite_eq_left c] at closed
        by_cases e : s = t
        · subst e; simp at closed
        · simp [e, wasOpen] at closed
      · rw [ite_eq_right c] at closed; simp [wasOpen] at closed
    | named t =>
      simp only [step] at closed ⊢
      by_cases c : (f.slot t).held = true
      · rw [ite_eq_left c] at closed
        by_cases e : s = t
        · subst e; simp [wasOpen] at closed
        · simp [e, wasOpen] at closed
      · rw [ite_eq_right c] at closed; simp [wasOpen] at closed
    | done t =>
      simp only [step] at closed ⊢
      by_cases pos : 0 < (f.slot t).entries
      · rw [ite_eq_left pos] at closed ⊢
        by_cases c : (f.slot t).held = false ∧ (f.slot t).entries - 1 = 0
        · rw [ite_eq_left c] at closed ⊢
          by_cases e : s = t
          · subst e; simp [freeSlot]
          · simp [freeSlot, e, wasOpen] at closed
        · rw [ite_eq_right c] at closed
          by_cases e : s = t
          · subst e; simp [wasOpen] at closed
          · simp [e, wasOpen] at closed
      · rw [ite_eq_right pos] at closed; simp [wasOpen] at closed
    | release t =>
      simp only [step] at closed ⊢
      by_cases c : (f.slot t).held = true
      · rw [ite_eq_left c] at closed ⊢
        by_cases u : (f.slot t).entries = 0
        · rw [ite_eq_left u] at closed ⊢
          by_cases e : s = t
          · subst e; simp [freeSlot]
          · simp [freeSlot, e, wasOpen] at closed
        · rw [ite_eq_right u] at closed
          by_cases e : s = t
          · subst e; simp [wasOpen] at closed
          · simp [e, wasOpen] at closed
      · rw [ite_eq_right c] at closed; simp [wasOpen] at closed
  have := inv_step f op h s nowFree
  exact ⟨this.1, this.2.1⟩

/-! ## The worked example -/

/-- The story's run: a table of one slot; A takes it and submits a read, releases it with the
read out, B's take finds slot 0 not free (nobody holds it after), the read completes, and the
slot is free again, for the next take. -/
theorem example_reuse_waits :
    let afterRelease := run (init 1) [.take 0, .named 0, .release 0]
    afterRelease.isFree 0 = false ∧
    ((step afterRelease (.take 0)).slot 0).held = false ∧
    (step afterRelease (.done 0)).isFree 0 = true ∧
    ((step (step afterRelease (.done 0)) (.take 0)).slot 0).held = true := by
  decide

end Nuclei.Files

/-!
# A list edit posted to the list's owner ends where the edit itself would

This file checks the mailbox a shard's task list is edited through when the shard was taken from
the thread that polled a task (`src/rt/sched.rs`, `Sched::pending` and `Sched::drop_future`;
`src/rt/context.rs`, `run_task_watched`): the thread that finds its shard taken does not edit the
list, it posts the edit, and the list's owner applies it later, after its own edits of other tasks.

## The story in plain words

A shard's list says which of its tasks are on it. A task's poll may ask for one edit of the list: a
first pending puts the task on it, a ready that ends a listed task takes it off. Meanwhile the
list's owner keeps editing it for its other tasks. The proofs below say that an edit of one task
and an edit of another task can be made in either order and leave the list the same, so an edit
posted now and applied after any number of the owner's edits of other tasks leaves the list exactly
as making it at once would have. The temporal side (who may edit when, the task's memory, the
shard's end) is `proofs/tla/listowner/ListOwner.tla`.
-/

namespace Nuclei.ListOwner

/-- Which tasks are on the list: one flag per task id. -/
abbrev Members := Nat → Bool

/-- One edit of the list: a task put on it, or taken off it. -/
inductive Edit where
  -- the task, on the list from now on
  | list (task : Nat)
  -- the task, off the list from now on
  | unlist (task : Nat)

/-- The task an edit is about. -/
def Edit.task : Edit → Nat
  -- either way, the task it names
  | .list t => t
  | .unlist t => t

/-- The flag an edit gives its task: on the list, or off it. -/
def Edit.flag : Edit → Bool
  -- put on it
  | .list _ => true
  -- taken off it
  | .unlist _ => false

/-- The list after an edit: the edited task's flag is the edit's, every other task's as it was. -/
def apply (m : Members) (e : Edit) : Members :=
  -- the edited task gets the edit's flag, the others keep theirs
  fun x => if x = e.task then e.flag else m x

/-- The list after several edits, made one after the other, the first first. -/
def applyAll (m : Members) (es : List Edit) : Members :=
  -- each edit applied to what the ones before it left
  es.foldl apply m

/-- An edit leaves every task but its own as it was. -/
theorem apply_other (m : Members) (e : Edit) (x : Nat) (h : x ≠ e.task) :
    apply m e x = m x := by
  -- the edited task is not `x`, so `x`'s flag is untouched
  simp [apply, h]

/-- An edit gives its own task its flag, whatever the list held. -/
theorem apply_own (m : Members) (e : Edit) : apply m e e.task = e.flag := by
  -- the edited task's flag is the edit's
  simp [apply]

/-- Two edits of different tasks can be made in either order: the list ends the same. -/
theorem apply_comm (m : Members) (e f : Edit) (h : e.task ≠ f.task) :
    apply (apply m e) f = apply (apply m f) e := by
  -- compare the two lists task by task
  funext x
  -- unfold both edits: each sets its own task's flag only
  simp only [apply]
  -- the cases on which task `x` is; it cannot be both, the two tasks differ
  by_cases h1 : x = e.task <;> by_cases h2 : x = f.task
  · exact absurd (h1.symm.trans h2) h
  · simp [h1]
    exact fun same => absurd same h
  · simp [h2]
    exact fun same => absurd same.symm h
  · simp [h1, h2]

/-- An edit of task `a` made first, then edits of other tasks, ends where those edits followed by
the edit of `a` end: so the edit can wait in the owner's mailbox while the owner edits the list for
its other tasks, and applying it then leaves the list as making it at once would have. -/
theorem defer (m : Members) (a : Edit) (es : List Edit) (h : ∀ f ∈ es, f.task ≠ a.task) :
    applyAll (apply m a) es = apply (applyAll m es) a := by
  -- one edit of the owner's at a time
  induction es generalizing m with
  | nil =>
    -- no edit of the owner's: both sides are the edit of `a` alone
    rfl
  | cons f rest ih =>
    -- the first edit of the owner's is of another task
    have hf : f.task ≠ a.task := h f (List.mem_cons_self ..)
    -- and so are the rest
    have hrest : ∀ g ∈ rest, g.task ≠ a.task := fun g hg => h g (List.mem_cons_of_mem _ hg)
    -- move the edit of `a` past `f`, then past the rest
    show applyAll (apply (apply m a) f) rest = apply (applyAll (apply m f) rest) a
    rw [apply_comm m a f (Ne.symm hf)]
    exact ih (apply m f) hrest

/-- The task the posted edit is about ends with the flag the edit gives it, whatever the owner did
for its other tasks in the meantime. -/
theorem deferred_task (m : Members) (a : Edit) (es : List Edit) (h : ∀ f ∈ es, f.task ≠ a.task) :
    applyAll (apply m a) es a.task = a.flag := by
  -- the edit of `a` may be taken as made last
  rw [defer m a es h]
  -- and then its own task's flag is the edit's
  exact apply_own _ a

/-- Every other task ends as the owner's own edits left it: the posted edit changes none of them. -/
theorem deferred_others (m : Members) (a : Edit) (es : List Edit) (h : ∀ f ∈ es, f.task ≠ a.task)
    (x : Nat) (hx : x ≠ a.task) : applyAll (apply m a) es x = applyAll m es x := by
  -- the edit of `a` may be taken as made last, and it leaves `x` as it was
  rw [defer m a es h]
  exact apply_other _ a x hx

end Nuclei.ListOwner

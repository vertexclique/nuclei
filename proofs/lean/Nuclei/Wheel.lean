/-!
# The timer wheel's arithmetic

This file checks the arithmetic the shard's timing wheel (`src/time/wheel.rs`) rests on: where
a timer is placed, when its slot is reached, and that moving a slot's timers down always takes
each one closer to firing, until it fires on level 0 exactly at its deadline.

## The story in plain words

The wheel counts milliseconds. It has six levels of 64 slots: a slot of level `l` is `64^l`
milliseconds wide, and the 64 slots of a level make a window of `64^(l+1)` milliseconds. The
wheel's time is `e`. A timer due at `w` goes to the lowest level whose window holds both `e`
and `w`, in the slot `w` falls in. Its slot comes when the wheel's time reaches the slot's
start. On level 0 the slot is one millisecond, the deadline itself, so the timer fires then.
Above, the timer is placed again: its deadline now lies in a smaller window with the wheel's
time, so it goes down at least one level.

A worked example, computed below: the wheel at 1000 and a timer due at 70000. The windows of
levels 0 to 1 (64 and 4096 ms) do not hold both, level 2's (262144 ms) does: level 2, slot
`70000 / 4096 % 64 = 17`, which starts at 69632. When the wheel gets there, the timer goes to
level 1 (`70000 - 69632 = 368` ms to go, inside one 4096 ms window), slot `70000 / 64 % 64 = 5`,
which starts at 69632 + 5 * 64 = 69952, and from there to level 0, slot 48: 70000, its deadline.

## What the model leaves out

The code finds the level with one exclusive-or and a count of leading zeros; this file states
the level by its definition (the lowest window holding both times), and a property test in
`src/time/wheel/tests.rs` checks that the two agree. The slot lists, the bitmaps and the slab
of entries are plain bookkeeping, checked by the unit and property tests (against a model
that keeps every deadline), not here.
-/

namespace Nuclei.Wheel

/-- The width of a level-`l` window: `64^(l+1)` milliseconds. -/
def range (l : Nat) : Nat := 64 ^ (l + 1)

/-- The width of a level-`l` slot: `64^l` milliseconds. -/
def width (l : Nat) : Nat := 64 ^ l

/-- Which level-`l` window a time falls in. -/
def win (l t : Nat) : Nat := t / range l

/-- Which slot of its level-`l` window a time falls in. -/
def slotOf (l t : Nat) : Nat := t / width l % 64

/-- The level a timer due at `w` goes to while the wheel's time is `e`: the lowest window
holding both, or the top level. (`level_by_windows` in the code.) -/
def level (e w : Nat) : Nat :=
  if win 0 e = win 0 w then 0
  else if win 1 e = win 1 w then 1
  else if win 2 e = win 2 w then 2
  else if win 3 e = win 3 w then 3
  else if win 4 e = win 4 w then 4
  else 5

/-- Where slot `s` of level `l` starts, in the window the wheel's time `e` is in. -/
def start (e l s : Nat) : Nat := win l e * range l + s * width l

/-! ## The level, case by case -/

/-- The level, with what picks it: the lowest window holding both times (the windows below
do not), or the top level when none below does. -/
theorem level_spec (e w : Nat) :
    (level e w = 0 ∧ win 0 e = win 0 w) ∨
    (level e w = 1 ∧ win 0 e ≠ win 0 w ∧ win 1 e = win 1 w) ∨
    (level e w = 2 ∧ win 1 e ≠ win 1 w ∧ win 2 e = win 2 w) ∨
    (level e w = 3 ∧ win 2 e ≠ win 2 w ∧ win 3 e = win 3 w) ∨
    (level e w = 4 ∧ win 3 e ≠ win 3 w ∧ win 4 e = win 4 w) ∨
    (level e w = 5 ∧ win 4 e ≠ win 4 w) := by
  unfold level
  by_cases h0 : win 0 e = win 0 w
  · simp [h0]
  by_cases h1 : win 1 e = win 1 w
  · simp [h0, h1]
  by_cases h2 : win 2 e = win 2 w
  · simp [h0, h1, h2]
  by_cases h3 : win 3 e = win 3 w
  · simp [h0, h1, h2, h3]
  by_cases h4 : win 4 e = win 4 w
  · simp [h0, h1, h2, h3, h4]
  · simp [h0, h1, h2, h3, h4]

/-- The level is one of the six. -/
theorem level_lt (e w : Nat) : level e w < 6 := by
  rcases level_spec e w with ⟨h, -⟩ | ⟨h, -⟩ | ⟨h, -⟩ | ⟨h, -⟩ | ⟨h, -⟩ | ⟨h, -⟩ <;> omega

/-- The definitions at each level, with the powers of 64 worked out. -/
macro "wheel_arith" : tactic =>
  `(tactic| (simp only [win, slotOf, start, range, width, Nat.reducePow, Nat.reduceAdd] at *;
             omega))

/-! ## Placement -/

/-- A timer later than the wheel's time, in the wheel's top window, goes to a slot after the
wheel's own slot on its level: never one the wheel has passed. -/
theorem placed_after (e w : Nat) (hlt : e < w) (htop : win 5 e = win 5 w) :
    slotOf (level e w) e < slotOf (level e w) w := by
  rcases level_spec e w with ⟨h, a⟩ | ⟨h, b, a⟩ | ⟨h, b, a⟩ | ⟨h, b, a⟩ | ⟨h, b, a⟩ | ⟨h, b⟩ <;>
    rw [h] <;> wheel_arith

/-- A timer's deadline lies inside its slot: the slot starts no later than the deadline, and
the deadline is before the slot's end. So a slot is reached before any of its timers is due. -/
theorem inside_slot (e w : Nat) (hlt : e < w) (htop : win 5 e = win 5 w) :
    start e (level e w) (slotOf (level e w) w) ≤ w ∧
      w < start e (level e w) (slotOf (level e w) w) + width (level e w) := by
  rcases level_spec e w with ⟨h, a⟩ | ⟨h, b, a⟩ | ⟨h, b, a⟩ | ⟨h, b, a⟩ | ⟨h, b, a⟩ | ⟨h, b⟩ <;>
    rw [h] <;> constructor <;> wheel_arith

/-! ## Reaching a slot -/

/-- On level 0 a slot is one millisecond: the slot of a timer starts at its deadline, so it
fires exactly on time. -/
theorem level_zero_exact (e w : Nat) (h : level e w = 0) :
    start e 0 (slotOf 0 w) = w := by
  rcases level_spec e w with ⟨_, a⟩ | ⟨h', -⟩ | ⟨h', -⟩ | ⟨h', -⟩ | ⟨h', -⟩ | ⟨h', -⟩
  · wheel_arith
  all_goals omega

/-- Moving a slot down: once the wheel's time is the start of the slot a timer above level 0
was in, placing the timer again puts it on a lower level. -/
theorem cascade_lowers (e w : Nat) (hlt : e < w) (htop : win 5 e = win 5 w)
    (hl : 0 < level e w) :
    level (start e (level e w) (slotOf (level e w) w)) w < level e w := by
  rcases level_spec e w with ⟨h, a⟩ | ⟨h, b, a⟩ | ⟨h, b, a⟩ | ⟨h, b, a⟩ | ⟨h, b, a⟩ | ⟨h, b⟩ <;>
    rw [h] at hl ⊢
  · omega
  all_goals
    rcases level_spec (start e _ (slotOf _ w)) w with
      ⟨h', c⟩ | ⟨h', d, c⟩ | ⟨h', d, c⟩ | ⟨h', d, c⟩ | ⟨h', d, c⟩ | ⟨h', d⟩ <;>
      rw [h'] <;> (try omega) <;> wheel_arith

/-! ## The next top window -/

/-- A deadline in the next top window, within the wheel's span (`64^6 - 1` ms ahead), goes to
the top level at a slot at or behind the wheel's own: the code reads such a slot as the next
window's, which starts no later than the deadline. -/
theorem next_window_wraps (e w : Nat) (hnext : win 5 w = win 5 e + 1)
    (hspan : w < e + range 5) :
    level e w = 5 ∧ slotOf 5 w ≤ slotOf 5 e ∧
      (win 5 e + 1) * range 5 + slotOf 5 w * width 5 ≤ w := by
  rcases level_spec e w with ⟨h, a⟩ | ⟨h, b, a⟩ | ⟨h, b, a⟩ | ⟨h, b, a⟩ | ⟨h, b, a⟩ | ⟨h, b⟩
  all_goals first
    | (exfalso; wheel_arith)
    | (refine ⟨h, ?_, ?_⟩ <;> wheel_arith)

/-! ## The worked example -/

/-- The wheel at 1000, a timer due at 70000: level 2, slot 17, starting at 69632; from there
level 1, slot 5, starting at 69952; from there level 0: the deadline. -/
theorem example_seventy_thousand :
    level 1000 70000 = 2 ∧ slotOf 2 70000 = 17 ∧ start 1000 2 17 = 69632 ∧
      level 69632 70000 = 1 ∧ slotOf 1 70000 = 5 ∧ start 69632 1 5 = 69952 ∧
      level 69952 70000 = 0 ∧ start 69952 0 (slotOf 0 70000) = 70000 := by
  decide

end Nuclei.Wheel

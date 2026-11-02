/-!
# Notify's owed wakes and its generation

This file checks two pieces of arithmetic `Notify` rests on (`src/sync/notify.rs`): how a drain
pass serves the `notify_one` calls it owes, and why a waiter's generation can never be mistaken
for the marker a `notify_one` or `notify_last` grant leaves on a node.

## The story in plain words

A pass owes some wakes (the `notify_one` calls since the last pass, plus one for a stored permit
when somebody waits). It walks the line from the front: a waiter still waiting is granted and
takes one owed wake; one that withdrew is let go and takes none. When the owed wakes run out it
stops; when the line runs out first, what is left over becomes the stored permit again. So every
owed wake is either served or left over for the permit, none is served twice, no more waiters are
served than wait, and something is left over only once every waiter still waiting was served.

A waiter's node carries the generation it was made in; a grant from `notify_one` or `notify_last`
replaces it with one of two markers at the top of the word. Generations are taken with the word's
top bit masked off, so a generation is always below half the word, and the markers are not.

A worked example, computed below: three owed wakes and a line of waiting, withdrawn, waiting
serves two and leaves one over, which becomes the permit.

## What the model leaves out

Who is in the line, the drain role, and how a grant reaches its waiter are the TLA+ model's part,
in `proofs/tla/notify/`. The stored permit is one bit: two wakes left over store one permit,
which is what `notify_one` promises (at most one permit is stored).
-/

namespace Nuclei.Notify

/-- Serves `owed` wakes from the front of `line` (`true`: still waiting, `false`: withdrew): how
many waiters it served, and how many owed wakes are left over. -/
def serve : Nat → List Bool → Nat × Nat
  | owed, [] => (0, owed)
  | 0, _ => (0, 0)
  | owed + 1, waiting :: rest =>
    if waiting then
      let r := serve owed rest
      (r.1 + 1, r.2)
    else
      serve (owed + 1) rest

/-- Every owed wake is served or left over: none is lost, none counted twice. -/
theorem served_and_left (owed : Nat) (line : List Bool) :
    (serve owed line).1 + (serve owed line).2 = owed := by
  induction line generalizing owed with
  | nil => simp [serve]
  | cons waiting rest ih =>
    cases owed with
    | zero => simp [serve]
    | succ k =>
      unfold serve
      split
      · have := ih k
        simp only
        omega
      · exact ih (k + 1)

/-- No more waiters are served than wait. -/
theorem served_le_waiting (owed : Nat) (line : List Bool) :
    (serve owed line).1 ≤ (line.filter id).length := by
  induction line generalizing owed with
  | nil => simp [serve]
  | cons waiting rest ih =>
    cases owed with
    | zero => simp [serve]
    | succ k =>
      unfold serve
      split
      · next h =>
        have := ih k
        simp [h]
        omega
      · next h =>
        have := ih (k + 1)
        simp [h]
        omega

/-- Something is left over only once every waiter still waiting was served. -/
theorem left_only_when_all_served (owed : Nat) (line : List Bool)
    (left : 0 < (serve owed line).2) : (serve owed line).1 = (line.filter id).length := by
  induction line generalizing owed with
  | nil => simp [serve]
  | cons waiting rest ih =>
    cases owed with
    | zero => simp [serve] at left
    | succ k =>
      unfold serve at left ⊢
      split
      · next h =>
        simp only [h] at left ⊢
        have := ih k left
        simp
        omega
      · next h =>
        simp only [h] at left ⊢
        have := ih (k + 1) left
        simp [this]

/-- The size of the word. -/
def W : Nat := 18446744073709551616

/-- The markers a `notify_one` and a `notify_last` grant leave (`usize::MAX`, `usize::MAX - 1`). -/
def byFirst : Nat := W - 1
def byLast : Nat := W - 2

/-- A generation as a node carries it: the counter with the top bit masked off. -/
def masked (g : Nat) : Nat := g % (W / 2)

/-- A generation is never one of the markers, whatever the counter. -/
theorem a_generation_is_never_a_marker (g : Nat) : masked g ≠ byFirst ∧ masked g ≠ byLast := by
  have : masked g < W / 2 := Nat.mod_lt g (by decide)
  unfold byFirst byLast W at *
  omega

/-! ## The owed word

Each kind of owed wake (`notify_one`'s, `notify_last`'s) is counted in one word that also holds
the generation the counts were made at (`Owed` in `src/sync/notify.rs`): the tag, the generation's
low 32 bits, then two counts of 16 bits, the current ones (made at the tag's generation) and the
older ones. A notifier adds its count in one compare-and-swap: of the word's generation, it is
current; of a newer one, the word's counts all turn older and the new generation's starts with
it; of an older one (its read of the generation came before a newer call's), it is older. A pass
reads the generation, then takes the word: a word of that generation's current counts are served
to its waiters, the older ones go to the waiters the pass expires, one each, and a word of a
newer generation (a broadcast and a call came after the read) goes back whole, added to the word
as it is by then, for the pass that broadcast asks for. What is left becomes the permit. A count that reaches its room's last value stays there, and a pass reads it as "every
waiter": it serves every waiter there is and leaves one over, as that many calls or more would.

Proved here: the word's fields come back as they were put in; a count made below the room is
never lost; the pass's split into current and older loses none, and a newer word it takes nothing
of; a word given back loses no count below the room; the expired take no more than
there are, and something is left over only once every expired one took one; a full count serves
every waiter still waiting and leaves the permit. -/

/-- The tag's room: a generation's low 32 bits. -/
def TAG : Nat := 4294967296

/-- A count's room in the word. -/
def ROOM : Nat := 65536

/-- A count's last value, where it stays: "every waiter". -/
def FULL : Nat := ROOM - 1

/-- A count held at `FULL` once it gets there. -/
def sat (c : Nat) : Nat := min c FULL

/-- An owed word, its fields apart. -/
structure Owed where
  tag : Nat
  cur : Nat
  old : Nat
deriving DecidableEq, Repr

/-- The fields fit their room. -/
def Owed.ok (w : Owed) : Prop := w.tag < TAG ∧ w.cur < ROOM ∧ w.old < ROOM

/-- The word as the code packs it: the tag above the current count above the older one. -/
def encode (w : Owed) : Nat := (w.tag * ROOM + w.cur) * ROOM + w.old

/-- The fields of a packed word. -/
def decode (n : Nat) : Owed := ⟨n / ROOM / ROOM, n / ROOM % ROOM, n % ROOM⟩

/-- The fields come back as they were put in. -/
theorem decode_encode (w : Owed) (h : w.ok) : decode (encode w) = w := by
  obtain ⟨tag, cur, old⟩ := w
  simp only [Owed.ok] at h
  obtain ⟨_, hc, ho⟩ := h
  simp only [decode, encode, Owed.mk.injEq]
  unfold ROOM at *
  refine ⟨?_, ?_, ?_⟩ <;> omega

/-- A packed word fits 64 bits. -/
theorem encode_lt (w : Owed) (h : w.ok) : encode w < 18446744073709551616 := by
  obtain ⟨tag, cur, old⟩ := w
  simp only [Owed.ok] at h
  obtain ⟨ht, hc, ho⟩ := h
  simp only [encode]
  unfold ROOM TAG at *
  omega

/-- Whether tag `g` is newer than tag `t`: ahead of it, as the generation wraps, by less than
half the tag's room. -/
def newer (g t : Nat) : Bool :=
  let d := (g + TAG - t) % TAG
  decide (0 < d) && decide (d < TAG / 2)

/-- A count made at tag `g` added to word `w` (`Owed::owe`). -/
def owe (w : Owed) (g : Nat) : Owed :=
  if w.tag = g then { w with cur := sat (w.cur + 1) }
  else if newer g w.tag then ⟨g, 1, sat (w.old + w.cur)⟩
  else { w with old := sat (w.old + 1) }

theorem sat_lt (c : Nat) : sat c < ROOM := by
  unfold sat FULL ROOM; omega

/-- A word that fits stays one that fits. -/
theorem owe_ok (w : Owed) (g : Nat) (hw : w.ok) (hg : g < TAG) : (owe w g).ok := by
  obtain ⟨ht, hc, ho⟩ := hw
  unfold owe
  split
  · exact ⟨ht, sat_lt _, ho⟩
  · split
    · exact ⟨hg, show 1 < ROOM by decide, sat_lt _⟩
    · exact ⟨ht, hc, sat_lt _⟩

/-- Below its room, a count made is never lost: the word holds one more than before. -/
theorem owe_counts (w : Owed) (g : Nat) (room : w.cur + w.old + 1 < FULL) :
    (owe w g).cur + (owe w g).old = w.cur + w.old + 1 := by
  unfold owe
  split
  · simp only [sat]; omega
  · split
    · simp only [sat]; omega
    · simp only [sat]; omega

/-- What a pass that read tag `g` after it took word `w` serves to the waiters of `g`. -/
def current (w : Owed) (g : Nat) : Nat := if w.tag = g then w.cur else 0

/-- What it gives to the waiters it expires: none of a word newer than `g`, which goes back. -/
def older (w : Owed) (g : Nat) : Nat :=
  if w.tag = g then w.old else if newer w.tag g then 0 else sat (w.old + w.cur)

/-- The pass's split of a word not newer than it loses no count below the room. -/
theorem split_counts (w : Owed) (g : Nat) (notNewer : newer w.tag g = false)
    (room : w.cur + w.old < FULL) : current w g + older w g = w.cur + w.old := by
  unfold current older
  split
  · omega
  · simp only [notNewer, Bool.false_eq_true, ite_false, sat]; omega

/-- A word newer than the pass gives it nothing: it goes back whole. -/
theorem newer_takes_nothing (w : Owed) (g : Nat) (ne : w.tag ≠ g) (isNewer : newer w.tag g = true) :
    current w g = 0 ∧ older w g = 0 := by
  unfold current older
  simp [ne, isNewer]

/-- Word `t`, taken, given back to word `w` (`Owed::give_back`, one compare-and-swap): of `w`'s
generation, its counts add to `w`'s; `w` of a newer one (a count came since), all of them are
older. -/
def giveBack (w t : Owed) : Owed :=
  if w.tag = t.tag then { w with cur := sat (w.cur + t.cur), old := sat (w.old + t.old) }
  else { w with old := sat (w.old + t.cur + t.old) }

/-- A word given back loses no count below the room. -/
theorem giveBack_counts (w t : Owed) (room : w.cur + w.old + t.cur + t.old < FULL) :
    (giveBack w t).cur + (giveBack w t).old = w.cur + w.old + t.cur + t.old := by
  unfold giveBack
  split
  · simp only [sat]; omega
  · simp only [sat]; omega

/-- The older counts the expired take, one each, and what is left over; a full count is taken by
every expired waiter and leaves one over. -/
def absorb (old expired : Nat) : Nat × Nat :=
  if old = FULL then (expired, 1) else (min old expired, old - min old expired)

/-- Below its room, every older count is taken or left over. -/
theorem absorbed_and_left (old expired : Nat) (room : old < FULL) :
    (absorb old expired).1 + (absorb old expired).2 = old := by
  unfold absorb
  by_cases h : old = FULL
  · omega
  · simp only [h, ite_false]; omega

/-- No more are taken than were expired. -/
theorem absorbed_le_expired (old expired : Nat) : (absorb old expired).1 ≤ expired := by
  unfold absorb
  by_cases h : old = FULL
  · simp only [h, ite_true]; omega
  · simp only [h, ite_false]; omega

/-- Something is left over only once every expired waiter took one. -/
theorem left_only_when_all_absorbed (old expired : Nat) (left : 0 < (absorb old expired).2) :
    (absorb old expired).1 = expired := by
  unfold absorb at *
  by_cases h : old = FULL
  · simp only [h, ite_true]
  · simp only [h, ite_false] at left ⊢; omega

/-- Serves `owed` wakes from the front of `line`, a full count as one more than the line holds. -/
def serveSat (owed : Nat) (line : List Bool) : Nat × Nat :=
  serve (if owed = FULL then line.length + 1 else owed) line

/-- With more wakes than the line, every waiter still waiting is served and one is left over. -/
theorem serve_more_than_line (line : List Bool) (owed : Nat) (more : line.length < owed) :
    (serve owed line).1 = (line.filter id).length ∧ 0 < (serve owed line).2 := by
  induction line generalizing owed with
  | nil => simp [serve]; omega
  | cons waiting rest ih =>
    cases owed with
    | zero => simp at more
    | succ k =>
      unfold serve
      split
      · next h =>
        have := ih k (by simp at more; omega)
        simp [h]
        omega
      · next h =>
        have := ih (k + 1) (by simp at more; omega)
        simp [h, this]

/-- A full count serves every waiter still waiting and leaves the permit. -/
theorem full_serves_every_waiter (line : List Bool) :
    (serveSat FULL line).1 = (line.filter id).length ∧ 0 < (serveSat FULL line).2 := by
  have full : (if FULL = FULL then line.length + 1 else FULL) = line.length + 1 := by simp
  unfold serveSat
  rw [full]
  exact serve_more_than_line line _ (by omega)

-- A broadcast lands between a notify_one's read and an older count: tag 7 holds two current,
-- a count of tag 8 turns them older and starts its own, and a pass of tag 8 serves one and
-- gives two to the waiters it expires.
example : owe ⟨7, 2, 0⟩ 8 = ⟨8, 1, 2⟩ := by decide
example : current ⟨8, 1, 2⟩ 8 = 1 ∧ older ⟨8, 1, 2⟩ 8 = 2 := by decide

-- The worked example: three owed, a line of waiting, withdrawn, waiting.
example : serve 3 [true, false, true] = (2, 1) := by decide

end Nuclei.Notify

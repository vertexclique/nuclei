/-!
# A vectored read's fill, in order

This file checks how a vectored read's bytes are shared out among its buffers
(`src/buf/vectored.rs`, `Vectored::set_len`): the kernel writes the bytes of one read into the
buffers' rooms in order, and the runtime then tells each buffer how many of its bytes it holds.

## The story in plain words

A vectored read hands the kernel the room of each buffer, in order: the bytes past what the
buffer held when the call took it, up to its capacity. The kernel fills the first room, then the
second, and so on, and answers how many bytes it wrote in all. The runtime shares that count out
again the same way: each room, in order, takes what is left of the count, up to its size.

Three things must hold for every list of rooms and every count, and are proved here:

- each room takes at most its size (`fill_fits`): no buffer is told it holds bytes past its
  capacity;
- the rooms take the whole count, or all of their room if the count is larger (`fill_total`):
  no byte the kernel wrote is lost, and none is made up;
- a room that is not filled is the last that takes anything (`fill_in_order`): a buffer gains
  bytes only once every buffer before it is full, which is the order the kernel wrote them in.

A worked example, computed below: rooms of 2, 0, 3 and 4 bytes and a read of 6 take 2, 0, 3 and 1.

## What the model leaves out

The bytes themselves: only the counts are modelled, since which byte lands where follows from
the counts and the order. The kernel's own rule, that a vectored read fills its descriptors in
order, is the kernel's (`readv(2)`, `preadv(2)`, `recvmsg(2)`, `WSARecv`), taken as given.
-/

namespace Nuclei.Fill

/-- What a read of `n` bytes puts in each room, the rooms in order: each takes what is left of
the count, up to its size. -/
def fill : List Nat → Nat → List Nat
  | [], _ => []
  | room :: rest, n => min room n :: fill rest (n - min room n)

/-- The sum of the counts. -/
def total : List Nat → Nat
  | [] => 0
  | x :: rest => x + total rest

/-- Each count is at most its room. -/
def fitsIn : List Nat → List Nat → Prop
  | [], [] => True
  | gain :: gains, room :: rooms => gain ≤ room ∧ fitsIn gains rooms
  | _, _ => False

/-- No room takes anything. -/
def nothing : List Nat → Prop
  | [] => True
  | gain :: gains => gain = 0 ∧ nothing gains

/-- A room that took less than its size is the last one that took anything. -/
def inOrder : List Nat → List Nat → Prop
  | [], [] => True
  | gain :: gains, room :: rooms => (gain < room → nothing gains) ∧ inOrder gains rooms
  | _, _ => False

/-- There is one count for each room. -/
theorem fill_length (rooms : List Nat) (n : Nat) : (fill rooms n).length = rooms.length := by
  induction rooms generalizing n with
  | nil => rfl
  | cons room rest ih =>
    show (fill rest (n - min room n)).length + 1 = rest.length + 1
    rw [ih]

/-- The rooms take the whole count, or all of their room if the count is larger. -/
theorem fill_total (rooms : List Nat) (n : Nat) : total (fill rooms n) = min n (total rooms) := by
  induction rooms generalizing n with
  | nil =>
    show 0 = min n 0
    rw [Nat.min_zero]
  | cons room rest ih =>
    show min room n + total (fill rest (n - min room n)) = min n (room + total rest)
    rw [ih]
    by_cases h : room ≤ n
    · rw [Nat.min_eq_left h]
      by_cases h2 : n - room ≤ total rest
      · rw [Nat.min_eq_left h2, Nat.min_eq_left (by omega : n ≤ room + total rest)]
        omega
      · rw [Nat.min_eq_right (by omega : total rest ≤ n - room),
          Nat.min_eq_right (by omega : room + total rest ≤ n)]
    · rw [Nat.min_eq_right (by omega : n ≤ room), Nat.sub_self, Nat.zero_min,
        Nat.min_eq_left (by omega : n ≤ room + total rest), Nat.add_zero]

/-- Each room takes at most its size. -/
theorem fill_fits (rooms : List Nat) (n : Nat) : fitsIn (fill rooms n) rooms := by
  induction rooms generalizing n with
  | nil => exact True.intro
  | cons room rest ih =>
    show min room n ≤ room ∧ fitsIn (fill rest (n - min room n)) rest
    exact ⟨Nat.min_le_left room n, ih _⟩

/-- Nothing left to share out, no room takes anything. -/
theorem fill_zero (rooms : List Nat) : nothing (fill rooms 0) := by
  induction rooms with
  | nil => exact True.intro
  | cons room rest ih =>
    show min room 0 = 0 ∧ nothing (fill rest (0 - min room 0))
    rw [Nat.min_zero]
    exact ⟨rfl, ih⟩

/-- A room that is not filled is the last one that takes anything. -/
theorem fill_in_order (rooms : List Nat) (n : Nat) : inOrder (fill rooms n) rooms := by
  induction rooms generalizing n with
  | nil => exact True.intro
  | cons room rest ih =>
    show (min room n < room → nothing (fill rest (n - min room n))) ∧
      inOrder (fill rest (n - min room n)) rest
    refine ⟨fun short => ?_, ih _⟩
    have h : n < room := by
      by_cases h : room ≤ n
      · rw [Nat.min_eq_left h] at short
        omega
      · omega
    rw [Nat.min_eq_right (Nat.le_of_lt h), Nat.sub_self]
    exact fill_zero rest

-- The worked example.
example : fill [2, 0, 3, 4] 6 = [2, 0, 3, 1] := by decide

/-- The mutation: every room taking the whole count, as a read that gave each buffer what the
kernel wrote in all would. It makes up bytes, which `fill_total` rules out. -/
def fillEach : List Nat → Nat → List Nat
  | [], _ => []
  | room :: rest, n => min room n :: fillEach rest n

-- The mutation breaks the count: two rooms of 4 and a read of 3 would hold 6 bytes.
example : total (fillEach [4, 4] 3) = 6 := by decide
example : total (fill [4, 4] 3) = 3 := by decide

end Nuclei.Fill

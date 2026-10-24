/-!
# The one-shot channel, one operation at a time

This file checks the bookkeeping of `oneshot` (`src/sync/oneshot.rs`), the channel that
carries one value from one sender to one receiver. It follows `Nuclei/FairSemaphore/`: the
model, the invariant and the theorems in one file, for a model of one thread doing one
whole operation at a time.

## The story in plain words

A one-shot channel starts empty and open. The sender writes the value into a cell and
raises two bits of one state word in the same step: the value is published, and the sender
is gone. If the receiver had closed first, the sender reads its own value back and the send
fails. The receiver looks at the word: with the value bit up it takes the value out (the
channel is done); with the sender gone and no value it answers an error; with an empty
close written down it answers an error at once. Closing or dropping the receiver raises the
close bit and writes down whether a value was already there: a close that saw the value
still owns it, so a later drop destroys it; a close that saw nothing never does.

A worked example:

1. The receiver polls once: nothing there yet, the poll stays pending.
2. The sender sends: the value sits in the cell, published, the sender spent.
3. The receiver polls again and takes the value: the channel answered it, the cell is empty.

Two more, one line each: sent then closed then the receiver dropped, and the value is
destroyed by that drop (somebody has to); closed empty then sent, and the value comes back
with the failed send while the receiver answers the error.

## What the model leaves out

It is one thread doing one whole operation at a time. In the code a send is a write, one
fetch-or and a wake, a poll is a look, a waker registration and a look again, and the two
halves interleave; the TLA+ model in `proofs/tla/oneshot` checks those interleavings,
including both register-then-look-again handshakes. Here every operation runs at once, as
it does once every concurrent step has landed. The wakers are not in this model either
(nobody waits sequentially), and the value is one unlabelled token: there is only ever one.
A `send` on a spent sender cannot be written (the method consumes the sender), so the
second-send question the theorems answer is: at most one send ever succeeds.
-/

namespace Nuclei.Oneshot

/-! ## The state -/

/-- What the receiver wrote down about its own close (`RxClose` in the code): it has not
closed (`open`), it closed after the value was sent (`withValue`), or it closed before any
value was sent (`empty`). -/
inductive Note where
  | open | withValue | empty
deriving DecidableEq

/-- What a receive answered (`Receiver` as a future). -/
inductive Answer where
  | pending | value | failure
deriving DecidableEq

/-- Everything the channel knows between two operations. -/
structure One where
  /-- The sender holds its half: until it sends or drops. -/
  txAlive : Bool := true
  /-- The receiver holds its half: until it answers or drops. -/
  rxAlive : Bool := true
  /-- The receiver's close note. -/
  note : Note := .open
  /-- VALUE_SENT: the sender ran its fetch_or (the value was written, published or back). -/
  valueSent : Bool := false
  /-- RX_CLOSED: the receiver closed or dropped. -/
  rxClosed : Bool := false
  /-- TX_DROPPED: the sender is gone: it sent, or was dropped. -/
  txDropped : Bool := false
  /-- The cell holds the value, written and not yet read. -/
  valueInCell : Bool := false
  /-- A send wrote the value (so it exists somewhere from then on). -/
  written : Bool := false
  /-- How many sends answered Ok. -/
  sends : Nat := 0
  /-- A refused send took the value back. -/
  valueBack : Bool := false
  /-- The receiver took the value out. -/
  got : Bool := false
  /-- The receiver's drop destroyed the value. -/
  droppedByRx : Bool := false
  /-- The last receive's answer. -/
  answer : Answer := .pending
deriving DecidableEq

/-- A fresh channel: both halves alive, nothing sent, nothing answered.
Example: `fresh` has `note = .open` and `sends = 0`. -/
def fresh : One := {}

/-- A Boolean as 0 or 1, for counting the places the value can be.
Example: `count true = 1`. -/
def count (b : Bool) : Nat := if b then 1 else 0

/-- In how many places the value sits at once: the cell, back with a refused send, taken by
the receiver, or destroyed by the receiver's drop. -/
def places (s : One) : Nat :=
  count s.valueInCell + count s.valueBack + count s.got + count s.droppedByRx

/-! ## The operations -/

/-- One operation on the channel. -/
inductive Op where
  /-- `Sender::send`: write the value, publish both bits, wake; Ok or Err. -/
  | send
  /-- A poll of the receiver (`poll`, `try_recv`, `blocking_recv`): take, answer, or stay
  pending. -/
  | recv
  /-- `Receiver::close`. -/
  | close
  /-- Dropping the receiver handle. -/
  | dropRx
  /-- Dropping the sender handle without sending. -/
  | dropTx

/-- One operation, at once. The write and the fetch-or of a send are one step here (see the
module docs), so a send either publishes (the receiver was open) or reads its value back
(the receiver had closed). A poll takes the value only when it was published and the note
is not `empty`; it answers the error on an `empty` note or a gone sender with no value; it
changes nothing while both the channel is open and the sender lives. -/
def step (s : One) : Op → One
  | .send =>
    -- A spent sender cannot send again (the code's method consumes it).
    if s.txAlive then
      if s.rxClosed then
        -- The receiver closed first: the value comes back with the Err.
        { s with txAlive := false, txDropped := true, valueSent := true, written := true,
                 valueBack := true }
      else
        -- The value is published and waits in the cell.
        { s with txAlive := false, txDropped := true, valueSent := true, written := true,
                 valueInCell := true, sends := s.sends + 1 }
    else s
  | .recv =>
    -- A completed receiver answers nothing more (the code panics; no run here asks).
    if s.rxAlive then
      if s.note = .empty then
        -- Closed with nothing sent: the error at once.
        { s with rxAlive := false, answer := .failure }
      else if s.valueSent then
        -- The value is this receiver's: it takes it out.
        { s with rxAlive := false, valueInCell := false, got := true, answer := .value }
      else if s.txDropped then
        -- The sender left without sending: the error.
        { s with rxAlive := false, answer := .failure }
      else s
    else s
  | .close =>
    -- A completed or already-closed receiver does nothing.
    if s.rxAlive ∧ s.note = .open then
      { s with rxClosed := true, note := if s.valueSent then .withValue else .empty }
    else s
  | .dropRx =>
    if s.rxAlive then
      -- The close part: only a still-open note does the fetch-or, and a note of
      -- `withValue` owns the value: the drop destroys it.
      if s.note = .open then
        if s.valueSent then
          { s with rxAlive := false, rxClosed := true, note := .withValue,
                   valueInCell := false, droppedByRx := true }
        else
          { s with rxAlive := false, rxClosed := true, note := .empty }
      else if s.note = .withValue then
        { s with rxAlive := false, rxClosed := true, valueInCell := false, droppedByRx := true }
      else
        { s with rxAlive := false, rxClosed := true }
    else s
  | .dropTx =>
    -- The sender handle's drop: one bit, nothing else.
    if s.txAlive then { s with txAlive := false, txDropped := true } else s

/-- The state after a whole sequence of operations. -/
def run (s : One) : List Op → One
  | [] => s
  | op :: ops => run (step s op) ops

/-! ## The invariant -/

/-- What holds of every state a fresh channel can reach.

A note on reading the fields: each one says a piece of the ownership story, the same story
the module docs of the code tell. `placed` is the heart of it: before any send the value is
nowhere, and after one it is in exactly one of the four places, forever. -/
structure Good (s : One) : Prop where
  /-- VALUE_SENT only goes up together with TX_DROPPED (one fetch_or). -/
  sentDropped (h : s.valueSent) : s.txDropped
  /-- Whoever raised TX_DROPPED spent the sender: it sent or it dropped. -/
  droppedGone (h : s.txDropped) : ¬s.txAlive
  /-- A refused send spent the sender too. -/
  backGone (h : s.valueBack) : ¬s.txAlive
  /-- A value the drop destroyed was a `withValue` close's value. -/
  droppedWith (h : s.droppedByRx) : s.note = .withValue
  /-- The close bit and the note move together. -/
  closedPair : s.rxClosed ↔ s.note ≠ .open
  /-- `withValue` is written down only after a send published. -/
  withValueSent (h : s.note = .withValue) : s.valueSent
  /-- An `empty` close never owned the cell. -/
  emptyCell (h : s.note = .empty) : ¬s.valueInCell
  /-- The cell is full only after a publish. -/
  cellSent (h : s.valueInCell) : s.valueSent
  /-- Before the write the value is nowhere; after it, in exactly one place. -/
  placed : places s = count s.written
  /-- At most one send ever succeeds (a second send fails: there is no sender left). -/
  sendsBound : s.sends ≤ 1
  /-- A successful send published. -/
  sendsSent : s.sends ≤ count s.valueSent
  /-- An `empty` note and a successful send exclude each other. -/
  emptySends (h : s.note = .empty) : s.sends = 0
  /-- A published value under a non-empty note was a successful send. -/
  outAndSent (hv : s.valueSent) (hn : s.note ≠ .empty) : s.sends = 1
  /-- A taken value was published under a non-empty note. -/
  gotFacts (h : s.got) : s.valueSent ∧ s.note ≠ .empty
  /-- A value answer took the value. -/
  valueAnswer (h : s.answer = .value) : s.got
  /-- An error answer: no send succeeded, so nothing was left behind. -/
  failureAnswer (h : s.answer = .failure) : s.sends = 0
  /-- A failure answer came from a close or from a sender that left without sending: with
  the sender alive and nobody closed, no receive can have answered an error. -/
  failureFrom (h : s.answer = .failure) : s.rxClosed ∨ s.txDropped
  /-- A gone receiver either answered (the value or an error) or closed: those are the
  only ways the receiver half ends. -/
  rxEnd (h : ¬s.rxAlive) : s.answer ≠ .pending ∨ s.rxClosed
  /-- A published value was written (the two rise together in the send). -/
  sentWritten (h : s.valueSent) : s.written
  /-- The value that came back with a refused send met an empty note. -/
  backEmpty (h : s.valueBack) : s.note = .empty
  /-- A taken value answered the receiver: that half is complete. -/
  gotDone (h : s.got) : ¬s.rxAlive
  /-- A destroyed value was destroyed by the receiver's drop: that half is complete. -/
  droppedDone (h : s.droppedByRx) : ¬s.rxAlive
  /-- A gone receiver never leaves a value in the cell. -/
  rxGoneCell (h : ¬s.rxAlive) : ¬s.valueInCell

/-- A fresh channel is good: every field is true of nothing sent and nothing answered. -/
theorem fresh_good : Good fresh := by
  refine { sentDropped := by simp [fresh], droppedGone := by simp [fresh],
           backGone := by simp [fresh], droppedWith := by simp [fresh],
           closedPair := by simp [fresh], withValueSent := by simp [fresh],
           emptyCell := by simp [fresh], cellSent := by simp [fresh],
           placed := by simp [fresh, places, count], sendsBound := by simp [fresh],
           sendsSent := by simp [fresh, count], emptySends := by simp [fresh],
           outAndSent := by simp [fresh], gotFacts := by simp [fresh],
           valueAnswer := by simp [fresh], failureAnswer := by simp [fresh],
           failureFrom := by simp [fresh],
           rxEnd := by simp [fresh],
           sentWritten := by simp [fresh], backEmpty := by simp [fresh],
           gotDone := by simp [fresh], droppedDone := by simp [fresh],
           rxGoneCell := by simp [fresh] }

/-! ## Every step keeps the invariant -/

/-- The facts a live sender gives about the state it steps from: nothing was published,
nothing was written, nothing came back, and no close bit is up (the note is not `open`
exactly then, and cannot be `withValue` without a publish). -/
theorem live_sender_facts (s : One) (g : Good s) (h : s.txAlive) :
    ¬s.valueSent ∧ ¬s.txDropped ∧ ¬s.written ∧ ¬s.valueBack ∧ ¬s.valueInCell ∧
      (s.rxClosed → s.note = .empty) := by
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_⟩
  · -- A publish would have dropped the sender.
    intro hv
    exact g.droppedGone (g.sentDropped hv) h
  · -- Raising TX_DROPPED spends the sender.
    intro hd
    exact g.droppedGone hd h
  · -- A written value sits in exactly one place (placed); every place spends the sender.
    intro hw
    have hp := g.placed
    rw [hw] at hp
    have hsum : count s.valueInCell + count s.valueBack + count s.got
        + count s.droppedByRx = 1 := by simpa [places, count] using hp
    by_cases hcell : s.valueInCell
    · exact g.droppedGone (g.sentDropped (g.cellSent hcell)) h
    by_cases hback : s.valueBack
    · exact g.backGone hback h
    by_cases hgot : s.got
    · exact g.droppedGone (g.sentDropped (g.gotFacts hgot).1) h
    by_cases hdrop : s.droppedByRx
    · exact g.droppedGone (g.sentDropped (g.withValueSent (g.droppedWith hdrop))) h
    · -- Every place is down, so the sum is zero against hsum: the write never happened.
      simp [count, hcell, hback, hgot, hdrop] at hsum
  · -- A refused send spent the sender.
    intro hb
    exact g.backGone hb h
  · -- A full cell was published.
    intro hc
    exact g.droppedGone (g.sentDropped (g.cellSent hc)) h
  · -- Closed with the sender alive: the note is not open (the pair), and withValue would
    -- need a publish; so it is empty.
    intro hrx
    have h1 : s.note ≠ .withValue := fun hnote =>
      g.droppedGone (g.sentDropped (g.withValueSent hnote)) h
    have h2 : s.note ≠ .open := (g.closedPair).mp hrx
    cases hnote : s.note
    · exact (h2 hnote).elim
    · exact (h1 hnote).elim
    · rfl

/-- Every operation keeps a good state good. -/
theorem step_good (s : One) (op : Op) (g : Good s) : Good (step s op) := by
  cases op with
  | send =>
    simp only [step]
    split
    · -- The sender lives: nothing was published, written, taken or destroyed yet.
      next htx =>
        obtain ⟨hvs, hvd, _, hvb, _, hclose⟩ := live_sender_facts s g htx
        have hcell : ¬s.valueInCell := fun hc => hvs (g.cellSent hc)
        have hgot : ¬s.got := fun hg => hvs (g.gotFacts hg).1
        have hdrop : ¬s.droppedByRx := fun hd => hvs (g.withValueSent (g.droppedWith hd))
        split
        · -- The receiver had closed: the send fails and the value comes back with the Err.
          next hrx =>
            have hnote : s.note = .empty := hclose hrx
            refine { sentDropped := by intro _; rfl,
                     droppedGone := by intro _ h; exact Bool.noConfusion h,
                     backGone := by intro _ h; exact Bool.noConfusion h,
                     droppedWith := g.droppedWith, closedPair := g.closedPair,
                     withValueSent := by intro _; rfl, emptyCell := g.emptyCell,
                     cellSent := by intro _; rfl,
                     placed := by simp [places, count, hcell, hgot, hdrop],
                     sentWritten := by intro _; rfl,
                     backEmpty := by intro _; exact hnote,
                     gotDone := g.gotDone, droppedDone := g.droppedDone,
                     sendsBound := g.sendsBound,
                     sendsSent := by
                       have hs := g.sendsSent; simp [count, hvs] at hs
                       simp <;> omega,
                     emptySends := g.emptySends,
                     outAndSent := by intro _ hn; exact absurd hnote hn,
                     gotFacts := by
                       intro h
                       exact ⟨rfl, (g.gotFacts h).2⟩,
                     valueAnswer := g.valueAnswer,
                     failureAnswer := g.failureAnswer,
                     failureFrom := by
                       intro h
                       rcases g.failureFrom h with _ | _
                       · exact Or.inl ‹s.rxClosed = true›
                       · exact Or.inr rfl,
                     rxEnd := g.rxEnd,
                     rxGoneCell := g.rxGoneCell }
        · -- The receiver was open: the value is published and waits in the cell.
          next hrx =>
            have hw : ¬s.written := by
              intro hw
              have hp := g.placed
              rw [hw] at hp
              simp [places, count, hcell, hvb, hgot, hdrop] at hp
            refine { sentDropped := by intro _; rfl,
                     droppedGone := by intro _ h; exact Bool.noConfusion h,
                     backGone := by intro _ h; exact Bool.noConfusion h,
                     droppedWith := g.droppedWith, closedPair := g.closedPair,
                     withValueSent := by intro _; rfl,
                     sentWritten := by intro _; rfl, backEmpty := g.backEmpty,
                     gotDone := g.gotDone, droppedDone := g.droppedDone,
                     emptyCell := by
                       intro h
                       have hne : s.note ≠ Note.open := by rw [h]; simp
                       exact absurd ((g.closedPair).mpr hne) hrx,
                     cellSent := by intro _; rfl,
                     placed := by simp [places, count, hvb, hgot, hdrop],
                     sendsBound := by
                       have hs := g.sendsSent; simp [count, hvs] at hs
                       show s.sends + 1 ≤ 1
                       omega,
                     sendsSent := by
                       have hs := g.sendsSent; simp [count, hvs] at hs
                       show s.sends + 1 ≤ 1
                       omega,
                     emptySends := by
                       intro h
                       have hne : s.note ≠ Note.open := by rw [h]; simp
                       exact absurd ((g.closedPair).mpr hne) hrx,
                     outAndSent := by
                       intro _ _
                       have hs := g.sendsSent; simp [count, hvs] at hs
                       show s.sends + 1 = 1
                       omega,
                     gotFacts := by
                       intro h
                       exact ⟨rfl, (g.gotFacts h).2⟩,
                     valueAnswer := g.valueAnswer,
                     failureAnswer := by
                       intro h
                       rcases g.failureFrom h with hc | hd
                       · exact absurd hc hrx
                       · exact absurd htx (g.droppedGone hd),
                     failureFrom := by
                       intro h
                       rcases g.failureFrom h with hc | hd
                       · exact absurd hc hrx
                       · exact Or.inr rfl,
                     rxEnd := g.rxEnd,
                     rxGoneCell := by
                       -- The cell fills only with the receiver alive: a gone receiver
                       -- answered or closed, and neither can hold here.
                       intro hgone _
                       rcases g.rxEnd hgone with hne | hc
                       · cases hcase : s.answer
                         · exact absurd hcase hne
                         · exact (hvs (g.gotFacts (g.valueAnswer hcase)).1).elim
                         · rcases g.failureFrom hcase with hc' | hd
                           · exact absurd hc' hrx
                           · exact absurd htx (g.droppedGone hd)
                       · exact absurd hc hrx }
    · -- A spent sender: nothing changes.
      exact g
  | recv =>
    simp only [step]
    split
    · -- The receiver is alive: the note, the value and the sender decide.
      split
      · -- An empty note: the error at once.
        refine { sentDropped := g.sentDropped, droppedGone := g.droppedGone,
                 backGone := g.backGone, droppedWith := g.droppedWith,
                 closedPair := g.closedPair, withValueSent := g.withValueSent,
                 emptyCell := g.emptyCell, cellSent := g.cellSent, placed := g.placed,
                 sendsBound := g.sendsBound, sendsSent := g.sendsSent,
                 emptySends := g.emptySends, outAndSent := g.outAndSent,
                 gotFacts := g.gotFacts,
                 sentWritten := g.sentWritten, backEmpty := g.backEmpty,
                 gotDone := by intro _ h; exact Bool.noConfusion h,
                 droppedDone := by intro _ h; exact Bool.noConfusion h,
                 valueAnswer := by intro h; exact Answer.noConfusion h,
                 failureAnswer := by
                   intro h
                   exact g.emptySends ‹s.note = .empty›,
                 failureFrom := by
                   intro _
                   exact Or.inl ((g.closedPair).mpr (by rw [‹s.note = .empty›]; simp)),
                 rxEnd := by intro _; exact Or.inl (by simp),
                 rxGoneCell := by
                   intro _ hcell
                   exact g.emptyCell ‹s.note = .empty› hcell }
      next hnote =>
        split
        · -- The value is published and the note is not empty: take it.
          refine { sentDropped := g.sentDropped, droppedGone := g.droppedGone,
                   backGone := g.backGone, droppedWith := g.droppedWith,
                   closedPair := g.closedPair, withValueSent := g.withValueSent,
                   emptyCell := by intro _ h; exact Bool.noConfusion h,
                   cellSent := by intro h; exact Bool.noConfusion h,
                   placed := by
                     have hw : s.written = true := g.sentWritten ‹s.valueSent = true›
                     have hgot0 : ¬s.got := fun hg => g.gotDone hg ‹s.rxAlive = true›
                     have hdb0 : ¬s.droppedByRx := fun hd => g.droppedDone hd ‹s.rxAlive = true›
                     have hvb0 : ¬s.valueBack := fun hb => hnote (g.backEmpty hb)
                     have hp := g.placed
                     simp [places, count, hw, hgot0, hdb0, hvb0] at hp ⊢
                   sentWritten := g.sentWritten, backEmpty := g.backEmpty,
                   gotDone := by intro _ h; exact Bool.noConfusion h,
                   droppedDone := by intro _ h; exact Bool.noConfusion h,
                   sendsBound := g.sendsBound, sendsSent := g.sendsSent,
                   emptySends := g.emptySends, outAndSent := g.outAndSent,
                   gotFacts := by intro _; exact ⟨‹s.valueSent = true›, hnote⟩,
                   valueAnswer := by intro _; rfl,
                   failureAnswer := by intro h; exact Answer.noConfusion h,
                   failureFrom := by intro h; exact Answer.noConfusion h,
                   rxEnd := by intro _; exact Or.inl (by simp),
                   rxGoneCell := by
                     intro _ hcell
                     exact Bool.noConfusion hcell }
        next hv =>
          split
          · -- The sender is gone without a value: the error.
            refine { sentDropped := g.sentDropped, droppedGone := g.droppedGone,
                     backGone := g.backGone, droppedWith := g.droppedWith,
                     closedPair := g.closedPair, withValueSent := g.withValueSent,
                     emptyCell := g.emptyCell, cellSent := g.cellSent, placed := g.placed,
                     sendsBound := g.sendsBound, sendsSent := g.sendsSent,
                     emptySends := g.emptySends, outAndSent := g.outAndSent,
                     gotFacts := g.gotFacts,
                     sentWritten := g.sentWritten, backEmpty := g.backEmpty,
                     gotDone := by intro _ h; exact Bool.noConfusion h,
                     droppedDone := by intro _ h; exact Bool.noConfusion h,
                     valueAnswer := by intro h; exact Answer.noConfusion h,
                     failureAnswer := by
                       intro _
                       have := g.sendsSent
                       simp [count, hv] at this
                       omega,
                     failureFrom := by intro _; exact Or.inr ‹s.txDropped = true›,
                     rxEnd := by intro _; exact Or.inl (by simp),
                     rxGoneCell := by
                       intro _ hcell
                       exact hv (g.cellSent hcell) }
          · -- Nothing yet: this poll stays pending.
            exact g
    · -- A completed receiver: nothing changes.
      exact g
  | close =>
    simp only [step]
    split
    · -- Alive and the note still open: the close runs and writes the note down.
      obtain ⟨hrx, hopen⟩ := ‹s.rxAlive ∧ s.note = .open›
      refine { sentDropped := g.sentDropped, droppedGone := g.droppedGone,
               backGone := g.backGone,
               droppedWith := by
                 intro h
                 have hn := g.droppedWith h
                 rw [hopen] at hn
                 exact absurd hn (by simp),
               closedPair := by
                 constructor
                 · intro _
                   by_cases hv : s.valueSent <;> simp [hv]
                 · intro _
                   rfl,
               withValueSent := by
                 intro h
                 by_cases hv : s.valueSent
                 · exact hv
                 · simp [hv] at h,
               emptyCell := by
                 intro h
                 by_cases hv : s.valueSent
                 · exact absurd h (by simp [hv])
                 · intro hcell
                   exact hv (g.cellSent hcell),
               cellSent := g.cellSent, placed := g.placed,
               sendsBound := g.sendsBound, sendsSent := g.sendsSent,
               emptySends := by
                 intro h
                 by_cases hv : s.valueSent
                 · exact absurd h (by simp [hv])
                 · have hs := g.sendsSent
                   simp [count, hv] at hs
                   omega,
               outAndSent := by
                 intro hv hn
                 exact g.outAndSent hv (by rw [hopen]; simp),
               gotFacts := by
                 intro h
                 obtain ⟨hv, _⟩ := g.gotFacts h
                 refine ⟨hv, ?_⟩
                 by_cases hvs : s.valueSent
                 · simp [hvs]
                 · exact absurd hv hvs,
               valueAnswer := g.valueAnswer, failureAnswer := g.failureAnswer,
               failureFrom := by intro _; exact Or.inl rfl,
               rxEnd := by
                 intro h
                 rcases g.rxEnd h with hne | hc
                 · exact Or.inl hne
                 · exact Or.inr rfl,
               sentWritten := g.sentWritten,
               backEmpty := by
                 intro h
                 rw [g.backEmpty h] at hopen
                 exact absurd hopen (by simp),
               gotDone := g.gotDone, droppedDone := g.droppedDone,
               rxGoneCell := g.rxGoneCell }
    · -- A completed or already-closed receiver: nothing changes.
      exact g
  | dropRx =>
    simp only [step]
    split
    · -- The receiver holds its half: the drop closes it and may destroy the value.
      split
      · -- The note was open: the drop writes it down from the value bit.
        split
        · -- It saw the value: the drop owns the value and destroys it.
          have hv : s.valueSent = true := ‹s.valueSent = true›
          have hopen : s.note = Note.open := ‹s.note = Note.open›
          have hrx : s.rxAlive = true := ‹s.rxAlive = true›
          refine { sentDropped := g.sentDropped, droppedGone := g.droppedGone,
                   backGone := g.backGone, droppedWith := by intro _; rfl,
                   closedPair := by
                     constructor
                     · intro _
                       simp
                     · intro _
                       rfl,
                   withValueSent := by intro _; exact hv,
                   emptyCell := by intro h; exact Note.noConfusion h,
                   cellSent := by intro h; exact Bool.noConfusion h,
                   placed := by
                     have hwr : s.written = true := g.sentWritten hv
                     have hgot0 : ¬s.got := fun h => g.gotDone h hrx
                     have hvb0 : ¬s.valueBack := fun h => by
                       have hne := g.backEmpty h
                       rw [hne] at hopen
                       exact Note.noConfusion hopen
                     have hdb0 : ¬s.droppedByRx := fun h => g.droppedDone h hrx
                     have hp := g.placed
                     simp [places, count, hwr, hgot0, hvb0, hdb0] at hp ⊢,
                   sendsBound := g.sendsBound, sendsSent := g.sendsSent,
                   emptySends := by intro h; exact Note.noConfusion h,
                   outAndSent := by
                     intro hv' _
                     exact g.outAndSent hv' (by rw [hopen]; simp),
                   gotFacts := by
                     intro h
                     exact ⟨(g.gotFacts h).1, by simp⟩,
                   valueAnswer := g.valueAnswer, failureAnswer := g.failureAnswer,
                   failureFrom := by intro _; exact Or.inl rfl,
                   rxEnd := by intro _; exact Or.inr rfl,
                   sentWritten := g.sentWritten,
                   backEmpty := by
                     intro h
                     have hne := g.backEmpty h
                     rw [hne] at hopen
                     exact Note.noConfusion hopen,
                   gotDone := by intro _ h; exact Bool.noConfusion h,
                   droppedDone := by intro _ h; exact Bool.noConfusion h,
                   rxGoneCell := by intro _ h; exact Bool.noConfusion h }
        · -- It saw nothing: the note is empty and owns nothing.
          have hvn : ¬(s.valueSent = true) := ‹¬(s.valueSent = true)›
          have hopen : s.note = Note.open := ‹s.note = Note.open›
          refine { sentDropped := g.sentDropped, droppedGone := g.droppedGone,
                   backGone := g.backGone,
                   droppedWith := by
                     intro h
                     rw [g.droppedWith h] at hopen
                     exact Note.noConfusion hopen,
                   closedPair := by
                     constructor
                     · intro _
                       simp
                     · intro _
                       rfl,
                   withValueSent := by intro h; exact Note.noConfusion h,
                   emptyCell := by
                     intro _ hcell
                     exact hvn (g.cellSent hcell),
                   cellSent := g.cellSent, placed := g.placed,
                   sendsBound := g.sendsBound, sendsSent := g.sendsSent,
                   emptySends := by
                     intro _
                     have hs := g.sendsSent
                     simp [count, hvn] at hs
                     omega,
                   outAndSent := by
                     intro h' _
                     exact absurd h' hvn,
                   gotFacts := by
                     intro h
                     exact absurd (g.gotFacts h).1 hvn,
                   valueAnswer := g.valueAnswer, failureAnswer := g.failureAnswer,
                   failureFrom := by intro _; exact Or.inl rfl,
                   rxEnd := by intro _; exact Or.inr rfl,
                   sentWritten := g.sentWritten, backEmpty := by intro _; rfl,
                   gotDone := by intro _ h; exact Bool.noConfusion h,
                   droppedDone := by intro _ h; exact Bool.noConfusion h,
                   rxGoneCell := by
                     intro _ hcell
                     exact hvn (g.cellSent hcell) }
      · -- The note was written down before: only the value part may run.
        split
        · -- A close that saw the value: the drop owns it and destroys it.
          have hw : s.note = Note.withValue := ‹s.note = Note.withValue›
          have hrx : s.rxAlive = true := ‹s.rxAlive = true›
          refine { sentDropped := g.sentDropped, droppedGone := g.droppedGone,
                   backGone := g.backGone, droppedWith := by intro _; exact hw,
                   closedPair := by
                     constructor
                     · intro _
                       rw [hw]
                       simp
                     · intro _
                       rfl,
                   withValueSent := g.withValueSent,
                   emptyCell := by
                     intro h
                     rw [h] at hw
                     exact Note.noConfusion hw,
                   cellSent := by intro h; exact Bool.noConfusion h,
                   placed := by
                     have hwr : s.written = true := g.sentWritten (g.withValueSent hw)
                     have hgot0 : ¬s.got := fun h => g.gotDone h hrx
                     have hvb0 : ¬s.valueBack := fun h => by
                       have hne := g.backEmpty h
                       rw [hne] at hw
                       exact Note.noConfusion hw
                     have hdb0 : ¬s.droppedByRx := fun h => g.droppedDone h hrx
                     have hp := g.placed
                     simp [places, count, hwr, hgot0, hvb0, hdb0] at hp ⊢,
                   sendsBound := g.sendsBound, sendsSent := g.sendsSent,
                   emptySends := by
                     intro h
                     rw [h] at hw
                     exact Note.noConfusion hw,
                   outAndSent := g.outAndSent, gotFacts := g.gotFacts,
                   valueAnswer := g.valueAnswer, failureAnswer := g.failureAnswer,
                   failureFrom := by intro _; exact Or.inl rfl,
                   rxEnd := by intro _; exact Or.inr rfl,
                   sentWritten := g.sentWritten,
                   backEmpty := by
                     intro h
                     have hne := g.backEmpty h
                     rw [hne] at hw
                     exact Note.noConfusion hw,
                   gotDone := by intro _ h; exact Bool.noConfusion h,
                   droppedDone := by intro _ h; exact Bool.noConfusion h,
                   rxGoneCell := by intro _ h; exact Bool.noConfusion h }
        · -- An empty note, or an open one (no close has run): nothing to destroy.
          -- The note is not open (the case we are in) and not withValue, so empty.
          have hon : ¬(s.note = Note.open) := ‹¬(s.note = Note.open)›
          have hwn : ¬(s.note = Note.withValue) := ‹¬(s.note = Note.withValue)›
          have hnoteEmpty : s.note = .empty := by
            cases hne : s.note
            · exact absurd hne hon
            · exact absurd hne hwn
            · rfl
          refine { sentDropped := g.sentDropped, droppedGone := g.droppedGone,
                   backGone := g.backGone, droppedWith := g.droppedWith,
                   closedPair := ⟨fun _ => hon, fun _ => rfl⟩,
                   withValueSent := g.withValueSent, emptyCell := g.emptyCell,
                   cellSent := g.cellSent, placed := g.placed,
                   sendsBound := g.sendsBound, sendsSent := g.sendsSent,
                   emptySends := g.emptySends, outAndSent := g.outAndSent,
                   gotFacts := g.gotFacts, valueAnswer := g.valueAnswer,
                   failureAnswer := g.failureAnswer,
                   failureFrom := by intro _; exact Or.inl rfl,
                   rxEnd := by intro _; exact Or.inr rfl,
                   sentWritten := g.sentWritten, backEmpty := g.backEmpty,
                   gotDone := by intro _ h; exact Bool.noConfusion h,
                   droppedDone := by intro _ h; exact Bool.noConfusion h,
                   rxGoneCell := by
                     intro _ hcell
                     exact g.emptyCell hnoteEmpty hcell }
    · -- A completed receiver: nothing changes.
      exact g
  | dropTx =>
    simp only [step]
    split
    · -- The sender still holds its half: one bit goes up and it is gone.
      refine { sentDropped := by intro _; rfl,
               droppedGone := by intro _ h; exact Bool.noConfusion h,
               backGone := by intro _ h; exact Bool.noConfusion h,
               droppedWith := g.droppedWith, closedPair := g.closedPair,
               withValueSent := g.withValueSent, emptyCell := g.emptyCell,
               cellSent := g.cellSent, placed := g.placed, sendsBound := g.sendsBound,
               sendsSent := g.sendsSent, emptySends := g.emptySends,
               outAndSent := g.outAndSent, gotFacts := g.gotFacts,
               valueAnswer := g.valueAnswer, failureAnswer := g.failureAnswer,
               failureFrom := by intro _; exact Or.inr rfl,
               rxEnd := g.rxEnd,
               sentWritten := g.sentWritten, backEmpty := g.backEmpty,
               gotDone := g.gotDone, droppedDone := g.droppedDone,
               rxGoneCell := g.rxGoneCell }
    · -- A spent sender: nothing changes.
      exact g

/-- A good state stays good whatever operations follow. -/
theorem run_good (s : One) (ops : List Op) (g : Good s) : Good (run s ops) := by
  -- By induction on the operations.
  induction ops generalizing s with
  -- No operation: the state is the one we started from.
  | nil => exact g
  -- One step keeps it good, and the rest by the fact for the rest.
  | cons op ops ih => exact ih _ (step_good s op g)

/-! ## The worked examples, computed -/

/-- The story of the module docs, step 1 to 3: a poll that stays pending, a send, and the
next poll takes the value. -/
theorem example_pending_then_taken :
    run fresh [.recv, .send, .recv] = { fresh with
        txAlive := false, rxAlive := false, valueSent := true, txDropped := true,
        valueInCell := false, written := true, sends := 1, got := true,
        answer := .value } := by rfl

/-- Sent, closed, receiver dropped: the close saw the value, so the drop destroys it (the
value is in exactly one place, the destroyed one). -/
theorem example_close_owns_the_value :
    (run fresh [.send, .close, .dropRx]).droppedByRx = true ∧
    places (run fresh [.send, .close, .dropRx]) = 1 := by
  refine ⟨by rfl, by rfl⟩

/-- Closed empty, then sent: the send fails and takes its value back, so nothing is left in
the channel. -/
theorem example_refused_send_takes_it_back :
    (run fresh [.close, .send]).valueBack = true ∧
    (run fresh [.close, .send]).valueInCell = false := by
  refine ⟨by rfl, by rfl⟩

end Nuclei.Oneshot

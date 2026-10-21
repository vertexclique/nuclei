# Proofs of nuclei

## What is checked, in plain words

nuclei's bounded channel (`nuclei::sync::mpsc::channel`) has a fixed number of slots. A sender
takes a slot before it puts a value in, and the receiver gives a slot back for every value it
takes out. When no slot is free, a sender waits in line, and the line is served strictly first
come, first served: slots that come free are set aside for the oldest waiter until it has all
it asked for, and nobody behind it, however little it asks for, gets a slot first. A waiter may
give up (its future is dropped); whatever was set aside or handed to it then goes on to the
next one. The receiver can close the channel, after which no new value gets in, and it can drop
it.

Many threads do this at once, and a bug in that kind of code often shows up only when their
steps happen in one particular order. A test can run millions of times without hitting that
order. A proof does not run the code and hope: it takes a precise description of the code and
checks every case the description allows. Two tools do the checking here, and they work
together:

- **TLA+** (checked by TLC) looks at several threads at once, every order of their steps, but
  only for small sizes: two slots, two senders with two operations each, and a receiver.
- **Lean** looks at one thread doing one whole operation at a time, but for every size and
  every sequence of operations.

What a check covers is said with it, and nothing beyond.

### A worked example: why a `try_send` must not take a free slot

Take a channel of one slot, two senders A and B, and the receiver R.

1. A sends a value: the slot is taken.
2. B sends and finds no slot. It joins the line and sleeps.
3. R takes A's value and gives its slot back. In the code that is one atomic addition to a
   counter. Handing the slot to B is a separate step a moment later: the "pass" over the line.
4. Between the two, A calls `try_send`, which never waits. It sees a free slot.

If A took it, it would get ahead of B, who has waited longer. The rule that prevents it: an
attempt that does not wait refuses while the line's `QUEUED` bit is up. The TLA+ model has a
configuration that takes this one rule away (`tla/mpsc/M_try_ignores_queue.cfg`), and TLC finds
the bad order and writes it down step by step (`tla/mpsc/out/M_try_ignores_queue_counterexample.txt`,
written by the run). With the rule in place (`tla/mpsc/BC_try.cfg`), TLC checks every order of
the same program and finds none.

### A second example: a waiter that leaves passes its share on

A channel of three slots, in the order things happen (this run is computed by Lean in
`lean/Nuclei/FairSemaphore/Theorems.lean`, `example_ops`):

1. Request 0 takes all 3 slots.
2. Request 1 asks for 2 and waits; request 2 asks for 1 and waits behind it.
3. Request 0 gives its 3 slots back. Request 1 is at the front: 2 slots are set aside for it
   and it is granted. The third goes to request 2. No slot is free.
4. Request 3 asks for 3 and waits. Request 1 gives its 2 back: both are set aside for request
   3, which still lacks one. Request 4 asks for 1 and waits behind request 3, even though 2
   slots came back a moment ago: they belong to request 3 now.
5. Request 3 is dropped. Its 2 set-aside slots are not lost: they go to request 4, the next in
   line, which is granted, and the one left over is free again.

The Lean proofs show that this is not luck of the example: for every sequence of operations,
no slot is lost or made up, and a dropped front waiter's share goes to the next waiter. The
TLA+ model checks the same with the steps of several threads interleaved, including the
moment a waiter is dropped while a pass is granting it.

## The tools

| tool | what it checks here | how far a check goes |
|------|---------------------|----------------------|
| TLA+, checked by TLC (`tla/`) | the bounded channel at the grain of its atomic steps: the semaphore's state word, the arrival stack, the waiting line and its set-aside, every waiter node, the drain pass, the waker handshake, cancellation, permits, the receiver's receive, close and drop | every order of every thread's steps, for the slots, senders and programs each configuration sets |
| Lean 4 (`lean/`) | the bookkeeping of the fair semaphore: conservation of slots, first come first served, the hand-on of a dropped waiter's share, the close | every sequence of operations and every size, for a model of one thread doing one whole operation at a time |

The loom models in `tests/sync_loom_mpsc.rs` complement both: they run the real Rust code
under every interleaving loom reaches (with the weak memory orderings of the atomics), for a
handful of operations.

## How to run them

TLA+ needs Java on `PATH` (CI uses Java 21). The runner fetches the pinned TLA+ tools (release
v1.7.4, checked against its SHA-256) on first use, or takes the jar named by `TLA2TOOLS`:

```sh
bash proofs/tla/run_tlc.sh mpsc                  # every configuration; rewrites tla/mpsc/tlc-run.txt
bash proofs/tla/run_tlc.sh mpsc BC_try M_try_ignores_queue   # only those
bash proofs/tla/run_tlc.sh all                   # every family
```

The model is written in PlusCal, an algorithm language that the TLA+ tools translate into a
TLA+ specification. The runner copies the family into a temporary directory and translates it
there before it checks it, so the translation is never stored in the tree and can never be
out of date. It compares each configuration's result with the family's `EXPECTED.txt` and
exits non-zero on any mismatch; a configuration expected to break something keeps its trace in
`tla/mpsc/out/` (not kept in the tree). `tla/mpsc/tlc-run.txt` records the last full run:
states, time and result of every configuration.

Lean: install elan once (`curl https://elan.lean-lang.org/elan-init.sh -sSf | sh`); it fetches
the Lean release `lean/lean-toolchain` names (4.35.0) on first use. Then:

```sh
bash proofs/lean/check.sh          # the scan for unfinished proofs, the build, the axiom check
bash proofs/lean/check.sh build    # `lake build`, failing on any warning
bash proofs/lean/check.sh sorry    # fail if `sorry` or `admit` appears in a Lean source
bash proofs/lean/check.sh axioms   # fail unless every theorem rests only on Lean's standard axioms
```

CI runs both: `.github/workflows/tla.yml` (every TLA+ family) and `.github/workflows/lean.yml`
(build, scan, axioms), each with its tools pinned by checksum.

## What the TLA+ model checks

`tla/mpsc/BoundedChannel.tla` is the algorithm; `tla/mpsc/MC_BoundedChannel.tla` holds the
programs the configurations run. One labelled step of the algorithm is one atomic step of the
code (a compare-and-swap, a fetch-and-add, a load), named after the Rust function it stands
for, with two exceptions the spec's header explains: the pass's look past withdrawn waiters at
the head of the line is one step, and so is the receiver's look at the queue and the sender
count. Every line carries a comment.

The properties:

| property | what it says |
|----------|--------------|
| `Conservation` | every slot is free, set aside, held by a grant, held as a permit, on its way back, or carrying a value in the queue: the counts add up to the channel's size, and the queue never holds more values than there are slots |
| `FifoFair` | a request that was already waiting when another began is never still waiting once that other one has its slots; a `try_` that took slots counts too, and so does a `reserve_many` |
| `CloseSemantics` | after the close no slot leaves the free count, and every operation that began after the close fails |
| `EndMeansDrained` | once a receive returned the end of the channel, no value ever goes in again |
| `ExactlyOnce` | every value put in is received once, or is still in the queue, or was thrown away by the receiver's drop |
| `PerSenderOrder` | each sender's values come out in the order it put them in |
| `Termination` (under weak fairness of every thread) | every thread finishes: no waiter whose request can be served sleeps forever, so no wakeup is lost; checked with the waiter's park and the grant's wake modelled step by step |
| deadlock freedom (TLC's own check, on every configuration that must pass) | no state where some thread is stuck while every other one is done |

The 42 configurations, listed with their expected result in `tla/mpsc/EXPECTED.txt` (each
`.cfg` file says in its first lines what it checks):

- **14 that must pass.** Ten check every safety property and deadlock freedom: three sends on
  one slot; a request for both slots among single sends; a request for both slots that gives
  them back unused; a request for both slots that is dropped after one poll; a single send that
  is dropped; a `try_` racing waiting sends; one sender's two values; the receiver closing in
  the middle, with and without a dropped request; the receiver dropped while sends wait. Four
  check `Termination` under weak fairness: plain sends, a dropped send, a request for both
  slots, and a close.
- **16 mutations that must fail.** Each takes one rule out of the algorithm and must break the
  property that rule protects: `try_ignores_queue` and `skip_unfit_front` (an attempt, or a
  pass, that serves a later request first) break `FifoFair`; `drop_pass_on` (a leaving
  waiter's share is dropped) breaks `Conservation`; `park_no_look`, `grant_no_wake`,
  `release_no_pass`, `exit_ignores_release`, `rx_no_recheck` and `close_keeps_set_aside` (a
  wakeup or a hand-over that never comes) end in a deadlock, and `park_no_look` and
  `grant_no_wake` also break `Termination`; `try_ignores_close` and `take_after_close` break
  `CloseSemantics`; `finish_early` breaks `EndMeansDrained`; `queue_lifo` breaks
  `PerSenderOrder`; `double_delivery` breaks `ExactlyOnce`.
- **12 witnesses that must fail.** Each claims an interesting case never happens, and must be
  proved wrong, which shows the search really reaches it: a share held between passes; a
  waiter that withdraws at the very moment a pass grants it; a grant that its future never
  looks at before the drop; the front waiter dropped while it holds a share; an attempt
  refused only because somebody waits; a sleeping waiter woken by a grant; a drain that passes
  again because something changed while it worked; the close giving a share back; a waiter
  failed by the close; a value sent after the close through a slot reserved before it; the
  receiver ending a closed channel because it went idle; and a grant that lands between a
  waiter's arrival and its park.

## What the Lean proofs check

`lean/Nuclei/FairSemaphore/Model.lean` models the fair semaphore with one thread doing one
whole operation at a time: acquire (wait if needed), try-acquire (never wait), release,
cancel and close, each followed by the pass. For every sequence of operations from a fresh
semaphore of any size, `lean/Nuclei/FairSemaphore/Theorems.lean` proves:

| theorem | what it says |
|---------|--------------|
| `conservation` | free slots, the set-aside and the slots held add up to the size, always |
| `waiting_means_nothing_free` | while anybody waits, no slot is free (so an attempt that does not wait takes nothing from a waiter) |
| `queue_in_arrival_order` | the line is ordered by arrival |
| `no_overtaking` | a step grants a waiter only after everybody who arrived before it and still waited, or that waiter's own cancellation |
| `fresh_only_when_nobody_waits` | a request granted without waiting found the line empty, or asked for nothing |
| `cancel_passes_share_on` | when the front waiter is dropped, the next one is granted, or holds the whole share that was set aside for the dropped one |
| `closed_admits_nothing` | after the close the semaphore stays closed and nobody is granted again |

and computes the worked example above (`example_after_release`, `example_share_held`,
`example_after_cancel`). `lean/Axioms.lean` prints the axioms each theorem rests on; the check
accepts only Lean's standard three (`propext`, `Quot.sound`, `Classical.choice`). No proof has a
`sorry` or an `admit`.

## What is not checked

- **Sizes beyond the configurations.** TLC checks every order of steps, but only for the
  programs the configurations set: at most two slots, two senders with at most two operations
  each, and a receiver with at most three operations. Larger cases are not checked by TLA+.
  Lean covers every size, but only one whole operation at a time.
- **The message queue.** The TLA+ model treats it as an in-order sequence, so
  `PerSenderOrder` is proved for an in-order queue only. Its lock-free implementation is
  kovan's, checked there; the queue the channel uses today can hand a sender's later value out
  before its earlier one under load (see the Ordering section of the `mpsc` module docs), and
  that is fixed separately.
- **The receiver's waker cell.** The TLA+ model treats it as one flag that a wake empties; its
  own handshake is covered by the loom models.
- **Memory ordering.** A TLA+ step is atomic and seen by every thread at once. Whether the
  orderings the Rust code gives its atomics are strong enough is what the loom models check,
  for small cases; neither TLA+ nor Lean says anything about it.
- **The rest of the channel.** The unbounded channel, weak senders and their upgrade,
  `Sender::closed`, `recv_many`, the waiter nodes' recycling, the waiting list's sweep of
  withdrawn nodes, and the destructors of values are not in these models.
- **How soon.** `Termination` says every waiter is eventually served under weak fairness; it
  says nothing about how long that takes, and nothing about a thread the scheduler stops for
  good.

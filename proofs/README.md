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
| TLA+, checked by TLC (`tla/`) | the bounded channel at the grain of its atomic steps: the semaphore's state word, the arrival stack, the waiting line and its set-aside, every waiter node, the drain pass, the waker handshake, cancellation, permits, the receiver's receive, close and drop; the one-shot channel, the runtime's task, the park handshake, the remote timer and the guard of an IO source's side the same way (sections below) | every order of every thread's steps, for the slots, senders and programs each configuration sets |
| Lean 4 (`lean/`) | the bookkeeping of the fair semaphore: conservation of slots, first come first served, the hand-on of a dropped waiter's share, the close; the one-shot channel's ownership, the task's references, future and output, and the timer wheel's arithmetic (sections below) | every sequence of operations and every size, for a model of one thread doing one whole operation at a time |

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
count. The message queue is a sequence of cells in the order sends claimed them: a send claims
its cell in one step and writes its value in a later one, as in kovan's segment queue, so the
receiver can find the oldest cell claimed and not written yet. It then takes nothing, does not
read the sender count, and waits for that send's wake, which comes after the write. Every
line carries a comment.

The properties:

| property | what it says |
|----------|--------------|
| `Conservation` | every slot is free, set aside, held by a grant, held as a permit, on its way back, or carrying a value in the queue: the counts add up to the channel's size, and the queue never holds more values than there are slots |
| `FifoFair` | a request that was already waiting when another began is never still waiting once that other one has its slots; a `try_` that took slots counts too, and so does a `reserve_many` |
| `CloseSemantics` | after the close no slot leaves the free count, and every operation that began after the close fails |
| `CloseReturnsShare` | once the channel is closed and no pass is running, nothing is set aside any more: the close's pass hands every share back to the free count |
| `EndMeansDrained` | once a receive returned the end of the channel, no value ever goes in again |
| `ExactlyOnce` | every value put in is received once, or is still in the queue, or was thrown away by the receiver's drop |
| `PerSenderOrder` | each sender's values come out in the order it put them in |
| `Termination` (under weak fairness of every thread) | every thread finishes: no waiter whose request can be served sleeps forever, so no wakeup is lost; checked with the waiter's park and the grant's wake modelled step by step |
| deadlock freedom (TLC's own check, on every configuration that must pass) | no state where some thread is stuck while every other one is done |

The 45 configurations, listed with their expected result in `tla/mpsc/EXPECTED.txt` (each
`.cfg` file says in its first lines what it checks):

- **14 that must pass.** Ten check every safety property and deadlock freedom: three sends on
  one slot; a request for both slots among single sends; a request for both slots that gives
  them back unused; a request for both slots that is dropped after one poll; a single send that
  is dropped; a `try_` racing waiting sends; one sender's two values; the receiver closing in
  the middle, with and without a dropped request; the receiver dropped while sends wait. Four
  check `Termination` under weak fairness: plain sends, a dropped send, a request for both
  slots, and a close.
- **18 mutations that must fail.** Each takes one rule out of the algorithm and must break the
  property that rule protects: `try_ignores_queue` and `skip_unfit_front` (an attempt, or a
  pass, that serves a later request first) break `FifoFair`; `drop_pass_on` (a leaving
  waiter's share is dropped) breaks `Conservation`; `park_no_look`, `grant_no_wake`,
  `release_no_pass`, `exit_ignores_release` and `rx_no_recheck` (a wakeup or a hand-over that
  never comes) end in a deadlock, and `park_no_look` and `grant_no_wake` also break
  `Termination`; `close_keeps_set_aside` (the close keeps what was set aside for the front
  waiter, so the channel can never go idle) breaks `CloseReturnsShare`;
  `try_ignores_close` and `take_after_close` break
  `CloseSemantics`; `finish_early` breaks `EndMeansDrained`; `queue_lifo` breaks
  `PerSenderOrder`, and so does `skip_busy` (the receiver passes over a cell still being
  written and takes a written one behind it, as the queue of kovan before 0.2 did: a sender's
  second value comes out first); `wake_before_write` (a send wakes the receiver before it
  writes, so a receiver that looked in between sleeps through the value) ends in a deadlock;
  `double_delivery` breaks `ExactlyOnce`.
- **13 witnesses that must fail.** Each claims an interesting case never happens, and must be
  proved wrong, which shows the search really reaches it: a share held between passes; a
  waiter that withdraws at the very moment a pass grants it; a grant that its future never
  looks at before the drop; the front waiter dropped while it holds a share; an attempt
  refused only because somebody waits; a sleeping waiter woken by a grant; a drain that passes
  again because something changed while it worked; the close giving a share back; a waiter
  failed by the close; a value sent after the close through a slot reserved before it; the
  receiver ending a closed channel because it went idle; a grant that lands between a
  waiter's arrival and its park; and the receiver parking while the oldest cell is still being
  written.

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
accepts only Lean's standard axioms (`propext`, `Quot.sound`, `Classical.choice`); the fair
semaphore and one-shot theorems need only the first two. No proof has a `sorry` or an
`admit`.

## The one-shot channel

`oneshot` (`tla/oneshot/Oneshot.tla`, `lean/Nuclei/Oneshot.lean`) gets the same treatment with
its own story: one value, one sender, one receiver. The sender publishes two bits of the state
word in one step; if the receiver had closed first the send fails and the value comes back.
The receiver registers its waker and looks again before sleeping, and so does a sender waiting
in `closed()`. Whoever owns the cell when the dust settles reads the value exactly once: the
receiver by receiving, the receiver's drop for a value nobody took, or the sender of a failed
send.

A worked example: the receiver polls once and stays pending; the sender sends; the next poll
takes the value (computed by Lean, `Nuclei.Oneshot.example_pending_then_taken`). Two more in
one line each: sent, closed, receiver dropped, and the close owns the value so the drop
destroys it; closed empty, then sent, and the failed send takes its value back.

The TLA+ family checks 23 configurations: 9 that must pass (sends, drops, a close, parking
races, and liveness under weak fairness), 8 mutations that each take one rule away (a wake
lost or early, a value read twice or dropped, a send past the close, both rechecks) and 6
witnesses that prove the search reaches the interesting cases. The Lean file proves the
ownership story for every sequence of operations: the value sits in exactly one of its four
places, at most one send succeeds, and every answer agrees with what the channel did. What the
models leave out is said in each file's header: the waker cells are single flags (their
handshake is loom's), and the value is one unlabelled token.

## The task

`task` (`tla/task/Task.tla`, `lean/Nuclei/Task.lean`) checks the runtime's task
(`src/rt/task/`): one allocation holding a future, one state word of flags and a count of
references, reached through a runnable, any number of wakers and one handle. A runner pops the
runnable and polls the future once. A wake that lands during the poll only marks the task, and
the runner puts it back in its queue after the poll, so the future is never polled twice at
once. The handle awaits the output, cancels the task (an idle one is scheduled once more, so a
runner drops its future), or lets go of it. Whoever lets go of the last reference frees the
memory, and never before the future and the output are gone.

A worked example (computed by Lean, `Nuclei.Task.example_detached_last_waker`): spawn; run,
Pending, the future keeps a waker; detach; wake by reference, which makes a new runnable; run
again, and the future drops that waker during the poll. The run's reference is now the last
one and nobody holds the handle, so nothing can wake the task or await it: the run drops the
future, then frees the memory. The established task crate frees the memory there and leaves
the future behind, never dropped. The TLA+ model found it (the mutation `pending_leaks_future`
brings that behaviour back and must fail), and nuclei's task drops the future in its last
decrement instead.

The TLA+ family checks 26 configurations: 11 that must pass (the handle awaiting, detaching,
dropped or cancelling, with one or two runners, wakes by value and by reference, a future that
wakes itself, a waker dropped without a wake, and liveness under weak fairness), 9 mutations
that each take one rule away (a wake during the poll that schedules a second runnable breaks
`OnePoll`; a run that drops a wake from its own poll, and a handle that sleeps without looking
again, end in a deadlock; freeing the memory while the handle lives, and a wake by reference
that makes a runnable without its own reference, break `NothingAfterFree`; a cancel that drops
the future itself breaks `FutureOnce`; a detach that skips the last-reference free, and a
completion that leaves the output behind, break `CleanEnd`; the established crate's leak
breaks `FreedAfterFuture`), and 6 witnesses: a wake during the poll, a cancel during the poll,
a cancel found in the queue, a registration of the handle's waker meeting a notification, the
last waker closing an idle task, and the handle woken from its sleep.

The Lean file proves, for every sequence of operations and any number of wakers: the count of
references is exactly the runnable plus the wakers; the future is dropped at most once, and a
runnable always has a live future; the output is used at most once; the memory is freed at
most once and only after the future and the output are gone; freed memory is never touched
again (every operation needs a reference or the handle, and none is left); and memory nobody
refers to is freed. The loom models in `src/rt/task/loom_tests.rs` run the real code under
weak memory orderings, with a calibration that loses a wake and must fail.

What the models leave out: the Lean model runs one whole operation at a time, so a poll is
never seen in progress (that is TLA+'s part); the TLA+ model treats the future as one step per
poll and the run queue as a count; the awaiter's waker cell is modelled by TLA+ only. The task
theorems also rest on `Classical.choice`, which the arithmetic tactic brings in for its case
analysis.

## The park handshake

`park` (`tla/park/Park.tla`) checks how a shard goes to sleep without losing a wake
(`src/rt/handshake.rs`, `src/rt/shard.rs`). A shard with nothing to run (after a short spin that
only looks at its inbox) publishes that it parks, looks at its inbox once more, and only then
sleeps until its poller is notified. A
thread that gives it work pushes the work, then looks whether the shard parks; if it does, it
swaps the flag back and notifies, so only the first of a burst of wakes notifies. Every access
of the pair is sequentially consistent, so whichever publication comes second, the look after
it sees the other: the shard finds the work, or the waker finds the shard parked.

The family checks 8 configurations: 3 that must pass (one waker, three wakers racing, and
liveness under weak fairness), 2 mutations that must deadlock (the shard sleeping without its
look after publishing; a waker looking before it pushes), and 3 witnesses (a notification, a
look that finds the work, a notification that lands while the shard is awake). TLA+ steps are
sequentially consistent, so the model checks the protocol; the loom models in
`src/rt/handshake.rs` check the code, with two calibrations that must lose a wake (no look
after publishing; the pair relaxed with no fence).

## A timer for a thread that runs no shard

`timer` (`tla/timer/Timer.tla`) checks how a sleep polled on another runtime's thread is fired
by a nuclei shard (`src/time/sleep.rs`). The sleep cannot touch the shard's wheel, so it posts
an arm request into the shard's mailbox and waits on a small shared cell (a fired flag and a
waker slot). The shard applies its mailbox in order, arms the timer, and when the deadline
passes sets the flag, wakes the waker and removes the entry. Dropping the sleep marks the cell
cancelled, takes the waker out and posts a cancel; the shard removes the entry if it has it. The
family checks 10 configurations: 4 that must pass (awaited and dropped, each with every safety
property and with liveness), 3 mutations (a fire that leaves its entry in the wheel and a
cancel that does not remove it break `NothingLeft`; a drop that leaves its waker in the cell
breaks `NoWakeAfterDrop`), and 3 witnesses (a drop after the timer was armed, a drop before
the shard applied the arm request, a fire that wakes the task). A dropped sleep's deadline may
never come within a run, so a cancel that did not remove its entry shows as a leak, as it would
be for a far deadline. The waker slot's register-then-look-again handshake guards a waker that
changed between polls; one task cannot show that, and it is checked by the loom models of the
sync module's waker cell.

## The guard of an IO source's side

`side` (`tla/side/Side.tla`) checks how a poll and the shard's driver share one side of an IO
source without losing an edge (`src/io/side.rs`, `src/io/source/read.rs`). The side's word has
a busy bit (someone holds it for one step) and a ready bit (an edge came while it was held).
Bytes arrive in the kernel, each with an edge for the driver. The driver takes a free side and
serves it: the edge clears the readiness cache, and with a reader waiting it reads into the
read-ahead and wakes the reader, or marks the cache blocked. Finding the side held, the driver
sets the ready bit and goes on; if the holder let go before the bit landed, it takes the side
after all. The reader holds the side for one step (it takes the read-ahead, reads the kernel
itself unless the cache says it would block, or registers its waker) and, as it lets go, looks
at the ready bit: if set it does the driver's part itself, then its own step again.

The family checks 11 configurations: 3 that must pass (two and three bytes with
`Conservation`, no byte lost or made up, and `Exclusive`, one holder at a time; and liveness
under weak fairness), 4 mutations (a driver that drops an edge on a held side, a holder that
lets go without looking at the ready bit and an edge that leaves the cache blocked each end in
a deadlock; a hold that refuses a free side carrying a flag breaks `Termination`), and 4
witnesses (a flag, a reader doing the driver's part, a read the driver completes, a read the
reader makes itself). Bytes are a count and there is one reader: several tasks waiting on one
side share its waker list, which the driver wakes whole. The loom models in `src/io/side.rs`
check the code's word with a calibration that must lose the edge (a driver that skips a held
side).

## The timer wheel

`lean/Nuclei/Wheel.lean` checks the arithmetic of each shard's timing wheel
(`src/time/wheel.rs`): six levels of 64 slots, one millisecond wide on level 0 and 64 times
wider on each level up. A timer goes to the lowest level whose window holds both the wheel's
time and the timer's deadline, in the slot the deadline falls in; when the wheel's time reaches
that slot, a level-0 timer fires and a higher one is placed again. For every wheel time and
every deadline the file proves: the slot is after the wheel's own on its level (the wheel has
not passed it); the deadline lies inside the slot, so the slot comes no later than the
deadline; on level 0 the slot is the deadline itself, so a timer fires exactly on time; placing
a timer again from its slot's start puts it on a lower level, so it always reaches level 0; and
a deadline in the next top window goes to a slot the code reads as the next window's, which
starts no later than the deadline. It computes a worked example: the wheel at 1000 and a timer
due at 70000 go through level 2, slot 17 (from 69632), level 1, slot 5 (from 69952), and fire
at 70000. Every theorem rests on `propext` and `Quot.sound` only.

The code finds a timer's level with one exclusive-or and a count of leading zeros; the Lean
file states the level by its definition, and a property test checks the two agree. The slot
lists, the bitmaps and the slab of entries are checked by unit tests and by a property test
that runs random arms, cancels, resets and advances against a model keeping every deadline: a
timer fires in the advance that first reaches its deadline, never earlier, never later, once.
The wheel has no unsafe code and runs on one thread, so it has no TLA+ model and no loom model.

## What is not checked

- **Sizes beyond the configurations.** TLC checks every order of steps, but only for the
  programs the configurations set: at most two slots, two senders with at most two operations
  each, and a receiver with at most three operations. Larger cases are not checked by TLA+.
  Lean covers every size, but only one whole operation at a time.
- **The message queue.** The TLA+ model treats it as a sequence of cells in claim order, each
  claimed by a send before it is written, so `PerSenderOrder` is proved for a queue that hands
  cells out in claim order. Its lock-free implementation is kovan's, checked there (kovan's
  own TLA+ model of the segment queue checks that order).
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

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
  only for small sizes: two slots, one or two senders with one to three operations each, and a
  receiver.
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
| TLA+, checked by TLC (`tla/`) | the bounded channel at the grain of its atomic steps: the semaphore's state word, the arrival stack, the waiting line and its set-aside, every waiter node, the drain pass, the waker handshake, cancellation, permits, the receiver's receive, close and drop; the one-shot channel, the runtime's task, the park handshake, the remote timer, the guard of an IO source's side, the reads and writes from a task's own buffer and a source's operations on its shard's ring the same way (sections below) | every order of every thread's steps, for the slots, senders and programs each configuration sets |
| Lean 4 (`lean/`) | the bookkeeping of the fair semaphore: conservation of slots, first come first served, the hand-on of a dropped waiter's share, the close; the one-shot channel's ownership, the task's references, future and output, the timer wheel's arithmetic and a ring's file slots (sections below) | every sequence of operations and every size, for a model of one thread doing one whole operation at a time |

The loom models in `src/sync/loom_tests/mpsc.rs` complement both: they run the real Rust code
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
receiver can find the oldest cell claimed and not written yet. It then takes nothing, reads the
sender count, which that send keeps above zero, and waits for that send's wake, which comes
after the write. Every line carries a comment.

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

The 48 configurations, listed with their expected result in `tla/mpsc/EXPECTED.txt` (each
`.cfg` file says in its first lines what it checks, and why its size is enough):

- **14 that must pass.** Ten check every safety property and deadlock freedom: three sends from
  two senders on one slot; a request for both slots racing the other sender's two sends; the
  same request putting nothing in, so both slots go on to the sends behind it; a request for
  both slots that is dropped after one poll, racing two sends; a single send that is dropped,
  racing two sends on one slot; a `try_` racing waiting sends; one sender's two values; the
  receiver closing in the middle, racing a request for both slots that gives them back unused
  and two sends, or a dropped request for both slots, a send after it and one more send; the
  receiver dropped while sends wait. Where a run is about one request, one sender makes it and
  the other only sends, once or twice; each `.cfg` says why that is enough for the bugs the
  run guards against. Four check `Termination` under weak fairness, on smaller programs, as a
  liveness check costs far more per state: one sender's three sends on one slot; one sender's
  send, dropped send and send; a request for both slots racing one send; one sender's send and
  request for both slots, with the receiver closing.
- **20 mutation runs that must fail.** Each of 18 mutations takes one rule out of the
  algorithm and must break the property that rule protects: `try_ignores_queue` and
  `skip_unfit_front` (an attempt, or a pass, that serves a later request first) break
  `FifoFair`; `drop_pass_on` (a leaving waiter's share is dropped) breaks `Conservation`;
  `park_no_look`, `grant_no_wake`, `release_no_pass`, `exit_ignores_release` and
  `rx_no_recheck` (a wakeup or a hand-over that never comes) end in a deadlock, and
  `park_no_look` and `grant_no_wake` run a second time, against `Termination`, which they
  break; `close_keeps_set_aside` (the close keeps what was set aside for the front waiter, so
  the channel can never go idle) breaks `CloseReturnsShare`; `try_ignores_close` and
  `take_after_close` break `CloseSemantics`; `finish_early` breaks `EndMeansDrained`;
  `queue_lifo` breaks `PerSenderOrder`, and so does `skip_busy` (the receiver passes over a
  cell still being written and takes a written one behind it, as the queue of kovan before 0.2
  did: a sender's second value comes out first); `wake_before_write` (a send wakes the
  receiver before it writes, so a receiver that looked in between sleeps through the value),
  `flag_before_write` (a send looks at the receiver's flag before it writes) and
  `raise_after_look` (the receiver says it waits only after its look again) end in a
  deadlock; `double_delivery` breaks `ExactlyOnce`.
- **14 witnesses that must fail.** Each claims an interesting case never happens, and must be
  proved wrong, which shows the search really reaches it: a share held between passes; a
  waiter that withdraws at the very moment a pass grants it; a grant that its future never
  looks at before the drop; the front waiter dropped while it holds a share; an attempt
  refused only because somebody waits; a sleeping waiter woken by a grant; a drain that passes
  again because something changed while it worked; the close giving a share back; a waiter
  failed by the close; a value sent after the close through a slot reserved before it; the
  receiver ending a closed channel because it went idle; a grant that lands between a
  waiter's arrival and its park; the receiver parking while the oldest cell is still being
  written; and a send that leaves the receiver's waker in its cell, because the receiver has
  not said it waits yet.

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
only looks at its inbox, first look before its clock starts: `Spin::run`, `src/rt/shard/spin.rs`) publishes that it parks, looks at its inbox once more, and only then
sleeps until its poller is notified. A
thread that gives it work pushes the work, then looks whether the shard parks; if it does, it
swaps the flag back and notifies, so only the first of a burst of wakes notifies. Every access
of the pair is sequentially consistent, so whichever publication comes second, the look after
it sees the other: the shard finds the work, or the waker finds the shard parked.

The family checks 21 configurations. `Park.tla`: 6 that must pass (one waker, three wakers
racing, liveness under weak fairness, a timer of the shard's own, its liveness, and a
`Proactor::wake` that ends a `Proactor::wait` wherever it lands), 4 mutations that must
deadlock (the shard sleeping without its look after publishing; a waker looking before it
pushes; the drive waiting on the poller after a timer it fired; a `Proactor::wake` that
notifies only a shard that parks already), and 4 witnesses (a notification, a look that finds
the work, a notification that lands while the shard is awake, a timer fired on the way to
park). `ShardStop.tla` checks a shard that stops while other threads push to it: a pusher
that sees the shard stopped drops what its inbox holds, and the stopping shard drains pass
after pass until nothing more comes (a timer's waker, a channel its tasks held), so no
runnable is left to keep itself and the shard for good. The inbox has one taker at a time:
the shard holds the role until its last pass, a pusher drains only if it takes the role, and
every drainer looks at the inbox again after it lets go. 2 that must pass (with `OneTaker`,
only the role's holder takes), 5 mutations (a pusher looking before it pushes, one that leaves
its runnable, a single drain pass, and a drainer that lets go without looking again break
`NothingLeft`; a pusher draining without the role breaks `OneTaker`), and 3 witnesses (a
pusher that drains, a second pass, a drainer taking the role back). TLA+ steps are
sequentially consistent, so the model checks the protocol; the loom models in
`src/rt/handshake.rs` check the code, with two calibrations that must lose a wake (no look
after publishing; the pair relaxed with no fence).

## A shard's inbox

`inbox` (`tla/inbox/Inbox.tla`) checks the queue other threads hand a shard its runnables
through (`src/rt/inbox.rs`), linked through the tasks so a push allocates nothing, with the
shard's park around it, at the grain of each atomic access. A pusher clears its task's link,
swaps it in as the head, links the old head to it, then wakes the shard if it parks; each task
is in the queue at most once and is pushed again as soon as it was taken. The shard walks from
its own end past a stub node, hands a task out only once it read the task's link, and pushes
the stub behind the last task before handing it out; between a push's swap and its link the
queue is busy. Empty is the head on the stub, which the shard looks at once more after it says
it parks. 11 configurations: 3 that must pass (two pushers three rounds each, three pushers two
rounds each, liveness), with `NoDuplicate` (no task handed out that is not queued), `Counted`,
`EmptyAtSleep` (a shard asleep with the head on the stub has no task queued) and no deadlock;
the three-pusher one checks each state once up to the pushers' names (a symmetry set), as the
pushers run the same code; 4 mutations (a pusher that leaves its task's old link, and a last
task handed out with the shard's end left on it, break `NoDuplicate`; a park whose look is at
the head alone breaks `EmptyAtSleep`; sleeping without the look again deadlocks); and 4
witnesses (a busy queue, the stub pushed behind the last task, tasks behind the shard's end with
the head on the stub, a sleep a push ends). TLA+ is
sequentially consistent: the unit tests in `src/rt/inbox.rs` run four pushing threads against
the taker. The same queue holds a `JoinSet`'s woken members (`src/join_set.rs`), linked through
each slot's waker: its `queued` flag keeps a waker in the queue at most once, as the scheduled
bit does a task, and the set (`&mut self`) is the one taker; there the model's park is the set's
awaiter, which a pusher wakes after its link.

## The blocking pool

`pool` (`tla/pool/Pool.tla`) checks a runtime's blocking pool (`src/rt/blocking.rs`): the
jobs, the threads that take them and the slots idle threads wait in, at the grain of each
atomic access. A submitter pushes its job (into the inbox's queue, checked in `inbox`), then,
if the idle count says a thread waits, claims an idle slot and wakes its thread, else starts a
thread while the pool has room. A thread with nothing to take counts itself idle, marks its
slot idle and looks at the queue once more; at its keep-alive it frees its slot, leaves the
thread count and looks once more, taking back a job no thread was started for. 9
configurations: 3 that must pass (two submitters and two threads that may time out, with
`RunOnce`, `Bounded`, `OneOwner` and `IdleCounted`, the idle count never below zero; liveness
there and with one thread), 2 mutations that break `AllRan` (a thread that waits without the
look after going idle, a leaving thread without its last look), and 4 witnesses (a wake, a look
that finds a job, a time-out, a job taken back). The unit tests in `src/rt/blocking.rs` run a
submit landing as the only thread goes idle (the first mutation fails it) and submitters on
threads against threads that keep timing out.

`tla/pool/PoolIndex.tla`, in the same family, checks the index of idle slots the network-path
work plans for the pool (`Pool::submit`, `Pool::wake_one` and `Pool::work`, not changed yet):
a bitmap beside the slots, one bit per slot, so a submitter finds a candidate with loads of the
bitmap and tries the compare-and-swap (idle to woken) on that slot only, instead of on every
slot from a hint. A bitmap rather than a queue of idle slots: finding a candidate takes loads
only, a bit set twice is still one bit, and its size is fixed. A thread sets its bit after it
marks its slot idle and before its look at the queue; only a slot's own thread writes its bit,
and it clears it after taking its slot back at its look, and before its slot's compare-and-swap
at the end of its wait, since a slot freed first may be another thread's at once. A set bit that
is stale sends the submitter on to the next candidate; with none left it starts a thread while
there is room. 20 configurations: 10 that must pass (two submitters with a pool of two threads,
and of three, with `RunOnce`, `Bounded`, `OneOwner`, `IdleCounted`, `IndexComplete`, the bitmap
misses no waiting thread, and `IndexTight`, a set bit names an idle or claimed slot or one whose
thread is about to clear it; `AllRan` with and without the keep-alive exit and at the cap of one
thread; the same with loads of the bitmap that may read any value and today's full scan as the
fallback; and two mutations that lose no job: a woken slot's bit never cleared, and, with
sequentially consistent loads, a submitter that gives up when it finds no candidate), 7
mutations (a bit set after the look, a submitter's claim followed by its own clear of the bit,
and, with loads that may read stale values, a submitter that gives up or only starts a thread at
the cap, each lose a job, breaking `AllRan`; a submitter that trusts the bit and stores woken
without the compare-and-swap breaks `IdleCounted`; a bit never cleared breaks `IndexTight`; a
bit cleared after the slot is freed erases another thread's bit, breaking `IndexComplete`), and
3 witnesses (a submitter sent to start a thread by a thread idle before its bit, a stale bit
whose compare-and-swap loses, a wake through the bitmap). The model reads the bitmap one slot's
bit per step, which covers a scan over several words, each word with its own load.

## Task blocks given back to their thread

`remote` (`tla/remote/RemoteFree.tla`) checks how a task's block freed on another thread than
its owner's goes back to the owner (`src/mem/mod.rs`), at the grain of each atomic access.
Each thread that allocates holds a record: a list other threads push the blocks they free onto,
linked through the blocks, and a count that bounds it (in the code, the bytes of the blocks in
it, at most 16 KiB; the model counts blocks against an abstract bound). A freeing thread counts itself in, gives
up at the bound, reads the head, gives up if the list is closed, and swaps its block in only if
the head is still the one it read. The owner takes the whole list when its cache has no block,
and as its thread ends swaps the list out and the head to closed in one step, frees all it
holds, then lets the record go; the next thread to claim the record opens it again. 11
configurations: 4 that must pass (three blocks, two owners one after the other and two freeing
threads, with `ListConsistent`, `ClosedEmpty`, `BoundedList`, `OpenWhileOwned`, `CountOK` and
`AllFreed`, every block back with the system once every thread is done, each state checked once
up to the names of the blocks and of the freeing threads; the same with a bound of one; and
`Terminates`, every thread ending, with two blocks under a bound of two and under a bound of one,
which between them still lose a compare-and-swap to another push, find the list full and find it
closed), 4 mutations (a push that swaps its block in whatever the head is, and a
close in two steps, each break `ListConsistent`; a record let go before its list is closed breaks
`OpenWhileOwned`; a push that ignores the count breaks `BoundedList`), and 3 witnesses (a push
that finds the list closed, one that finds it full, the next owner taking back a block the first
allocated). The unit tests in `src/mem/mod.rs` give a block back across threads, refuse
one at a closed record and at the bound (a take-back that never runs fails two, a push past the
bound one); `tests/zero_alloc.rs` counts no allocation for blocking jobs, detached spawns onto a
worker and the pool's file jobs.

## The remote wake handshake on a store buffer

`handshake` (`tla/handshake/Handshake.tla`) checks the Dekker pair of the park handshake
(`src/rt/handshake.rs`, measured in `src/syscore/linux/bench/wake_handshake.rs`) on a machine
whose stores wait in a store buffer, as x86's do: each thread's buffer in order, the flush of
its oldest store at any time, and each store, load, fence and swap of the pair. A store passed by
a later load of another word is the one reordering of the pair a weaker ordering lets through.
7 configurations: 2 that must pass (every access sequentially consistent, a store that waits for
its buffer and a push that is a locked read-modify-write; relaxed accesses with a sequentially
consistent fence on both sides), each with two wakers notifying once (`OneNotify`) and no wake
lost (`AllEnd`); 3 mutations that lose a wake (relaxed with no fence, as the measurement's
control lost wakes on hardware; a fence on the shard's side only; on the waker's side only);
and 2 witnesses (both sides see each other; a notification). `lean/Nuclei/Handshake.lean` proves
the two facts the pair takes from the total order: one load comes after the other side's store,
and of a burst of swaps of the raised flag exactly the first reads it up.

## A timer for a thread that runs no shard

`timer` (`tla/timer/Timer.tla`) checks how a sleep polled on another runtime's thread is fired
by a nuclei shard (`src/time/sleep.rs`). The sleep cannot touch the shard's wheel, so it posts
an arm request into the shard's mailbox and waits on a small shared cell (a fired flag and a
waker slot). The shard applies its mailbox in order, arms the timer, and when the deadline
passes sets the flag, wakes the waker and removes the entry. Dropping the sleep marks the cell
cancelled, takes the waker out and posts a cancel; the shard removes the entry if it has it. The
shard may go before the deadline (its runtime dropped, a worker stopped): as it goes it drains
its mailbox and lets go of its wheel, and every timer it held for a cell, armed or only asked
for, orphans the cell (`RemoteTimer::orphan`, through the wheel's `Armed` hold), whose waker is
woken, so the cell never keeps the task's waker for good while the task keeps the cell; a request
posted to a shard that went is drained by its poster the same way. The sleep, polled again, lets
the orphaned cell go and arms the timer where it is polled, here a second shard. The family
checks 17 configurations: 8 that must pass (awaited and dropped, with the shard staying and with
it free to go, each with every safety property and with liveness), 5 mutations (a fire that
leaves its entry in the wheel and a cancel that does not remove it break `NothingLeft`; a drop
that leaves its waker in the cell breaks `NoWakeAfterDrop`; a shard that goes and wakes nobody
breaks `NoStrandedWaker`, the cycle that kept a dropped runtime's shards after a handoff; a sleep
that waits on its orphaned cell breaks `Termination`), and 4 witnesses (a drop after the timer
was armed, a drop before the shard applied the arm request, a fire that wakes the task, a timer
armed again on the second shard after the first went). A dropped sleep's deadline may
never come within a run, so a cancel that did not remove its entry shows as a leak, as it would
be for a far deadline. The waker slot's register-then-look-again handshake guards a waker that
changed between polls; one task cannot show that, and it is checked by the loom models of the
sync module's waker cell.

## A paused clock that moves by itself

`clock` (`tla/clock/Clock.tla`) checks how a runtime's paused clock jumps to the earliest
armed timer when the runtime has nothing else to do (`src/rt/paused.rs`, the park path of
`src/rt/shard.rs`, the job count of `src/rt/blocking.rs`). Each shard publishes its deadline
and counts a park before it says it parks; parked, it claims the advance, looks at every shard
(parked with nothing pushed, noting its park count and deadline), looks again (the same,
unchanged), moves the clock to the earliest deadline and wakes the shards whose timers came
due. A shard that wakes, or that finds work as it is about to sleep, waits for an advance in
progress before it reads the clock. A blocking job wakes its task before it counts itself out
of the jobs in flight, then tries the advance too. The family checks five configurations that
must pass. Three check `ClockNeverPassesArmed` (no jump leaves an armed timer behind) and
`NoBadJump` (every jump comes over a moment, between its two looks, at which every shard was
idle, with no job in flight) at full size: two shards with a pusher and a blocking job; two
shards with a timer each, the scenario the races below need; three shards. With no blocking job
the shards are alike, so those two (and the races' own runs) check each state once up to the
shards' names (a symmetry set), and every run forgets a value nobody reads again (whether a
shard's own timer came due, the shard a push went to) once it was read. Two smaller ones
check both and, over every behaviour, the liveness property `TimersFire` (every armed timer
fires: no runtime hangs with its shards asleep and a timer armed): two shards with the blocking
job alone, and two shards with the pusher alone, whose timers the tasks they wake arm again. A
timer on each of two shards, or three shards, are too many states to check every behaviour, so
`TimersFire` is checked where one shard arms its timers. Then one mutation per rule (a deadline
published after the park, a busy path that does not wait for the advance, one look instead of
two, a woken shard that does not wait, no look at the jobs, no attempt at the park); and a
witness that the clock jumps at all. Writing the model found the first two of those races in the
first version of the code (a deadline published after the park, a busy path that did not wait);
the code was fixed before it was committed.

## The guard of an IO source's side

`side` (`tla/side/Side.tla`) checks how a poll and the shard's driver share one side of an IO
source without losing an edge or a wake, and without either ever waiting for the other
(`src/io/side.rs`, `src/io/source/read.rs`). The side's word has a busy bit (someone holds it
for one step), a ready bit (an edge came while it was held) and a waiting bit (a poll found it
held and waits to be woken). Bytes arrive in the kernel, each with an edge for the driver,
which takes the side with one fetch-or of busy and ready: free, it holds the side and serves
the edge (the cache is cleared, and with a reader waiting it reads into the read-ahead and wakes
the reader, or marks the cache blocked); held, the ready bit it left is the holder's to serve.
The reader takes the side with one fetch-or of busy: free, it takes the read-ahead, or (unless
the cache says the call would block) runs the driver's read at once into the read-ahead and
takes that, or registers its waker. Held, it stores its waker in the contended set and raises
the waiting bit; if that showed the side free, it takes the side once more, else it sleeps until
the holder wakes it. Letting go is one compare-and-swap from the word the holder took to zero;
when flags came meanwhile it fails, and the holder takes them with one swap, still holding: the
ready bit, it does the driver's part (and its own step again if it waited); the waiting bit, it
takes the contended wakers, which it wakes once it has let go.

16 configurations: 3 that must pass (two and three bytes with `Conservation`, no byte lost or
made up, and `Exclusive`, one holder at a time; and liveness under weak fairness), 7 mutations
(a driver that drops an edge on a held side, one that flags in a step of its own after looking,
a holder that lets go without serving the ready bit, an edge that leaves the cache blocked, a
holder that wakes nobody for the waiting bit, and a contended reader that sleeps without looking
whether the side came free each end in a deadlock; a take that refuses a free side carrying a
flag breaks `Termination`), and 6 witnesses (a flag, a reader doing the driver's part, a read
the driver completes on an edge, a reader's submit that runs the driver's read at once, a
contended reader woken by the holder, and a contended reader that finds the side free and takes
it). Bytes are a count and there is one reader: several tasks waiting on one side share its
waker list, which the driver wakes whole. The loom models in `src/io/side/loom_tests.rs` check
the code's word with two calibrations that must strand a thread (a driver's part that forgets
the reader's wake, and a contended poll that is never woken).

`Requests.tla`, in the same family, checks the work that needs both sides of a source, a
dropped handle's close in particular (`src/io/source/requests.rs`), against a holder on each
side that loads the descriptor and uses it. The dropper raises the close in the source's request
word and takes the read side with one fetch-or that also raises the side's request bit; holding
it, it tries the write side the same way, and with both it performs the close. Taking fewer, it
lets go, never waiting. Sides are taken in one order: a holder of the read side that sees the
request bit tries the write side; a holder of the write side lets go first, then tries the read
side once. The dropper hands its object to the source before it raises the close, the close's own
hold still up; a job of the blocking pool (a file's call, a `send_file` chunk, a ring's attach or
detach in its mailbox) takes a lease on the object while the handle lives and may use the
descriptor after the drop; the last of the close's hold and the leases drops the object, so no
descriptor of nuclei's own is ever made for a call. The close lingers: once the dropper is done, a
task drains what the handle wrote through the handle's own object, which it keeps open until
then, holding the write side; finding the close not performed yet,
it waits on the write side, and the performer wakes the write side's waiters rather than
forget them as it does the handle's others. 12 configurations: 2 that must pass
(`NoUseOfClosed`, no holder and no job with a lease uses a descriptor closed under it;
`PerformedOnce`; `Closed`, once everything else is done the close was performed and the object
dropped; and liveness), 7 mutations (a holder that ignores the request bit, or takes a raised bit
for its own, breaks `Closed`; a dropper that performs holding one side, a dropper that drops its
object before the close is done, and a close that drops it with a lease out break
`NoUseOfClosed`; a holder of the write side that tries the read side while still holding its own
livelocks, and a close that forgets the lingering drain's wake strands it, both breaking
`Termination`), and 3 witnesses (a holder performs the close; the last lease, not the close, drops
the object; the lingering drain waits before the close is performed). The Rust tests in
`src/io/source/tests/requests.rs` hold a side from the test and check the same on real sockets,
each with a mutation of the code that makes one of them fail.

## The side's guard biased to its owner

`side_biased` (`tla/side_biased/SideBiased.tla`) checks the owner-biased guard (`Side::try_hold`,
`Side::poll`, `Side::contend`, `Side::defer`, `Side::take_for_request` and
`Held::release_with` in `src/io/side.rs`; the heavy fence is `heavy` and `heavy_retried` in
`src/rt/handoff/fence.rs`), on a machine whose stores wait in a store buffer of two entries, as
x86's do. A socket's side has an owner, the thread running its home shard, which serves the
driver's edges. The owner takes the side with a plain store of its word `own`, a compiler fence
and a plain load of the foreign word `intent`: zero, it holds; one, it undoes `own` and takes
today's word instead. It lets go with a plain store of `own` and a plain load of `intent`; only
when `intent` is one does it read today's flag word, and then with a locked instruction, which
drains its buffer first, so the read cannot run ahead of its store. Finding work left there
(the waiting, ready, deferred or request bit), it takes the side again through today's word and
does the work as today's holder does. A foreign call (a poll, a defer or a request) raises
`intent` with a locked instruction, runs the heavy fence (every buffer drained at once) and reads
`own`: zero, it switches the side to shared mode for good and takes today's word; one, it leaves
its work for the owner (a poll its waker and the waiting bit, a defer or a request its bit),
looks once more at `own`, and a poll sleeps while a defer or a request goes on. In shared mode
everyone uses today's word, as `side` checks it. The monitor moves the home shard to a spare only
through the slot protocol: its claim while the owner is outside its shard, the heavy fence, its
take. The slot's own words are atomic here, since `handoff` checks them on the same store
buffers; the old owner acts as a foreign thread from then on.

A heavy fence may fail (`fence::heavy_retried` tries it eight times). A foreign call whose fence
failed reads nothing the fence was to order (`Side::unserved`): after a full fence of its own it
reads the owner's claim. With none, no owner can hold biased, so the side goes shared and is
taken through today's word; with one, the call raises `intent`'s UNFENCED bit, leaves its work in
the word (a poll its waker and the waiting bit) and nudges the owner's shard to take and let go
of the side (a nudge reaches the shard once the claim recorded it; a claimant not recorded yet
takes the side next anyway). The owner, letting go of `own` at its fallback or its release,
switches the side to shared when it sees UNFENCED (`Side::switch_if_unfenced`), so the woken poll
takes through the word.

40 configurations: 19 that must pass (the owner and one foreign thread polling twice; one making a
defer and a request; the owner and two foreign threads polling, and making requests; a handoff
mid-run beside a foreign thread; a handoff after which the old owner polls and makes a request;
fences that may fail, with one and two edges, a defer and a request, and a handoff; every fence
failing, with an owner claimed, with none, and with two foreign threads; each with `Exclusive`,
one holder at a time, `NoSpin`, no thread waits for another except asleep until woken,
`NoLostWork`, every edge served and no left bit up once everything is finished, and
`SharedAbsorbing`, shared mode for good, with no deadlock; and `Termination` under weak fairness,
without and with a handoff, with fences that may fail, and with every fence failing with and
without an owner), 13 mutations (no heavy fence on the foreign path, an owner that holds without
reading `intent`, an owner that trusts a mode still biased over `intent`, a foreign thread that
holds through its intent alone without switching, a move of the shard while its owner is inside
a take, a move that leaves the old owner's stores in its buffer, a call that reads `own` after a
failed fence, and a switch stored before the owner's `own := 0` each break `Exclusive`; a release
that never looks for left work, one that reads the flag word with a plain load, and a nudge that
never reaches the owner each end in a deadlock, a lost wake; a release that finds work but does
not take the side again breaks `NoLostWork`; an owner that never switches on UNFENCED leaves the
poll failing its fence for ever, breaking `Termination`), and 8 witnesses (a foreign call that
leaves its work to the owner, a switch to shared mode, a handoff, the owner's release taking the
side again to wake a waiting poll, and to do a deferred bit a foreign defer left, a failed fence
the owner's switch serves, a failed fence with no owner that goes shared, a nudge served). The
switch stored before `own := 0` breaks `Exclusive` because the model's hold lasts until that store;
on x86 the owner touches nothing between them, so it shows the rule rather than a lost update.

The plain read of the flag word (`M_release_plain_look`) is the design's first form, and the
model found it loses a wake: the owner's loads run ahead of its `own := 0`, read `intent` as one
and the flag word with no waiting bit yet; the poll then raises the bit and looks once more at
`own`, which memory still says is one, and sleeps with nobody to wake it. The bit is raised
after the foreign thread's heavy fence, so the two form a store-buffering pair, and the locked
read puts the fence on the owner's side, only on the release in flight when the first foreign
thread arrives (every later owner take falls back to today's word).

`Retire.tla`, in the same family, checks the planned plain operation count on a ring, before its
code is written (today `Source::op_out` and `op_done` in `src/io/source/ops.rs` still change
`ops_out` with sequentially consistent read-modify-writes, against the flag of `Source::retire`
in `src/io/source/requests.rs`). A ring's submit stores the count plus one and hands the operation to
the kernel; its `op_done`, on the ring the operation went to, stores the count minus one and
then, with a compiler fence only, reads the waiters' flags, and with the count at zero and a flag
up takes that waiter's waker and wakes it. A plain count needs one writer at a time, so a
source's re-home to another ring (`evict_now` and the re-attach) waits on the new ring's thread
until the count is zero before that ring submits: a side may still have an operation out on the
old ring. That wait and the retire's are the same: register the waker, store the flag, run the
heavy fence, read the count; zero, go on, else sleep until woken and look again. The retire's
request and the re-home's move are one step each (no submit of the old home after them, and
nothing assumed of how they order memory). 14 configurations: 6 that must pass (two and three
operations on one ring, and a re-home with an operation still out on the old ring, with one and
two operations there, with `NoLostUpdate`, once every store landed the count is the operations
really out, and `FreedAfterOps`, the object goes only once no operation of the source is out on
either ring, and no deadlock; and `Termination` with and without a re-home), 5 mutations (a
retire with no heavy fence, and one that reads the count before raising its flag, each let the
object go while the kernel holds an operation, breaking `FreedAfterOps`; an `op_done` that reads
the flags before its decrement breaks `Termination`; a new ring that submits before the count is
zero, and a re-home whose wait has no heavy fence, so the old ring's completion counts down a
count the new ring writes, each break `NoLostUpdate`), and 3 witnesses (a retire that sleeps, one
woken by `op_done`, and a re-home that waits for the old ring's last completion). The Windows
port keeps today's atomic count. The models take a foreign call's step as one step, leave the
side's contents to `side`, and take the deferred and request bits as work a holder does when it
takes them.

## Reads and writes from a task's own buffer

`lent` (`tla/lent/LentRead.tla`, `tla/lent/LentWrite.tla`, `tla/lent/LentFile.tla`,
`tla/lent/LentZc.tla`) checks `read_owned`, `write_owned` and `write_zero_copy` on streams
(`src/io/source/read.rs`, `src/io/source/write/lent.rs`), where the task hands the side its own
buffer and the driver reads into it or writes from it with no copy, and on files
(`src/fs/file/`), where the buffer goes to a job of the blocking pool or the ring, beside the
file's positional calls, syncs and direct IO. The guards are abstract here (a step under one runs
whole; `side` checks the side's).

The read: bytes arrive in order, each with an edge, and then the stream ends. The lending task
takes what another read left first (bytes, or the end); otherwise it lends its buffer, which
the driver fills at once if the socket may hold bytes or on a later edge, and wakes it; the
task takes the buffer back. It may go away while the buffer is lent: what the driver read into
it then becomes the read-ahead. A second task reads as usual until it reads the end. While a
filled buffer waits to be taken back, the driver reads nothing more, so a task that went away
hands its bytes on ahead of anything read after them. 12 configurations: 5 that must pass
(buffers of one and two bytes, a lender that may go away and one that stays, with
`Conservation`, no byte lost or made up; `Order`, each task gets its bytes in order;
`NoScribble`, the driver reads only into a buffer the side has; `AllTaken`, every byte is
taken; and liveness), 4 mutations (a lender that goes away and takes its bytes with it breaks
`Conservation`; reads that go on while a filled buffer waits break `Order`; a buffer taken back
while the side still counts it lent breaks `NoScribble`; a fill that wakes nobody ends in a
deadlock), and 3 witnesses (the driver fills the buffer on an edge, the lender's own poll fills
it at once, a lender that went away leaves its bytes to the read-ahead).

`LentReadRing.tla` checks the same read on a ring (`read_owned` and `lent_idle` in
`src/io/source/read.rs`, `submit_ops` and `op_done` in `src/io/source/read/ops.rs`), where a
read is an operation the kernel completes later and a side has one out at a time. A plain read
asks for one into a buffer of the pool and its task goes away while it is out; then a task reads
with its own buffer until the stream ends. Its first lend finds the operation out, so none goes
out for the buffer, and the operation completes into the read-ahead: the completion wakes the
lender, whose buffer comes back to take those bytes before it is lent again. 6 configurations: 3
that must pass (three bytes through a buffer of two and two through a buffer of one, with
`Conservation`, `Order`, `NoScribble` and `AllTaken`, the lender takes every byte; and
`Termination`), 2 mutations (a waiting buffer that stays lent while bytes wait in the read-ahead,
and a completion that wakes only the plain readers, both leave the lender waiting forever: a
deadlock), and 1 witness (the buffer comes back for the bytes the dropped read's operation
brought).

`LentDatagram.tla` checks two tasks' owned datagram receives on one socket (`read_owned_from` and
`reclaim` in `src/io/source/read.rs`, `op_done` in `src/io/source/read/ops.rs`). One task's buffer
waits lent while the driver brings a datagram into a buffer of its own, through an operation it
had out before the lend (a ring, a completion port): only the task whose buffer is lent, which
holds none, takes it back to copy that datagram in; a task holding its own buffer copies the
datagram into it. 5 configurations: 3 that must pass (with datagrams ahead and with every datagram
received into the lent buffer, with `OwnBuffer`, each task gets its own buffer back,
`NoLostBuffer` and `Conserved`, every datagram in one place; and `Termination`), 1 mutation (a
reclaim by a task holding its own buffer, the bug that gave a second task the first one's buffer,
breaks `OwnBuffer`), and 1 witness (a datagram ahead while a buffer waits lent and empty).

The write: the socket is full and a byte waits in the write-behind. The lending task lends a
buffer of three bytes, which the driver writes after the write-behind as the peer makes room,
and a later copied write waits until it is out. A lender that goes away has its write withdrawn
if none of it went out, and finished by the driver if some did, so the wire carries 1 2 3 4 5
or 1 5 and never part of the buffer. 12 configurations: 5 that must pass (sockets that hold one
and two bytes, a lender that may go away and one that stays, with `WireOrder`, the bytes in
order and the buffer whole or not at all; `Delivered`, everything written reaches the peer;
`NoScribble`; and liveness), 4 mutations (withdrawing a write that started and a copied write
that passes the lent buffer break `WireOrder`; a task that takes its buffer back once some of
it went out breaks `NoScribble`; an end that wakes nobody deadlocks), and 3 witnesses (a buffer
that goes out over several calls, the driver finishing the write of a task that went away, a
withdrawn write). A vectored write (`write_vectored_owned`) is this write: its buffers are lent
as one, and each call hands the kernel every byte not sent yet at once, so the model's rules are
its rules. On a file a vectored write is the lent write at the cursor of `LentFile.tla`, and a
vectored read on either is the lent read, filling each buffer's room before the next one's (an
order the property test `buf::tests::vectored::a_read_fills_rooms_in_order` checks).

The file (`LentFile.tla`) checks a file handle's reads, writes, syncs and seeks
(`src/fs/file/`: `mod.rs`, `positional.rs`, `orphans.rs`, `durable.rs`, `work.rs`) in five
scenarios, which a constant picks so that each configuration holds only the tasks its rules need.
In all of them the file has a cursor and an epoch, which every write and seek at the cursor moves
on, and its jobs run on the blocking pool or the ring of the task's shard, in any order; a write
on the ring may land in two parts, and a shard that goes has its ring's calls made on the pool,
from where they stopped.

- **cursor** (the first one checked): two tasks each read one cell at the cursor into their own
  buffers: a read waits for the write in flight, its job reads at the cursor, and its result
  counts only if the cursor and the epoch are still those it read at, else it reads again. A
  third task writes a cell from the pool, then one from its own buffer (which waits for the write
  in flight and then is that write), then seeks back to the start. Any task may go away while its
  job is out: a read job is cancelled or its result dropped, and a lent write still lands. A file
  opened to append makes its writes on the pool at its end, wherever the cursor was; once one
  landed the cursor is the end of it; and its writer appends once more after the seek. In Rust a
  seek needs the handle exclusively, so it cannot overlap a read; the model lets it, so the epoch
  rule is checked for any path that moves the cursor back.
- **positional**: a task writes cell 0 at an offset of its own (`write_at`) and another reads it
  (`read_at`), while the cursor writer writes cell 0 once, before, while or after the positional
  write is out. Neither positional call reads or moves the cursor or the epoch; each waits first
  for the cursor writes accepted before it. The positional writer may go away while its write is
  out: the write still lands, the file keeps it (`orphans`), and a cursor write after that waits
  for it. A change of a range of the file (`allocate`: reserve, punch a hole, zero) and each chunk
  of a copy into the file (`copy_range_to`) are positional writes in this scenario: both go
  through `Out` and `poll_ordered`, the file keeps one whose future went away, and the cut of
  `set_len` moves the generation on as a positional write's end does.
- **generation**: a cursor reader reads two cells through the read-ahead (a job reads ahead two
  cells), another one cell into its own buffer, while a task writes cell 1 at an offset of its
  own. When a positional write's task takes its end (or the file takes the end of one whose task
  went away), the file's generation moves on, and a cursor read counts a job's result, or hands
  out cells read ahead, only of the generation it began in.
- **sync**: the cursor writer's second copied write waits in the write-behind while the first is
  in flight, and a task syncs twice once both were accepted. A sync waits until the write-behind
  went out and every write in flight landed, then its job makes what the file holds durable, or
  fails (the device may fail it); a failed sync makes every later one fail, with no call. The
  sync of a range (`sync_range`) is such a sync with another call.
- **direct**: the file is opened for direct IO: a read job reads exactly the cell asked for, and
  each copied write is one kernel write of exactly its bytes, which waits for the write in
  flight instead of joining the write-behind.

In every scenario each job runs on the handle's own descriptor, holding a lease on its object
until it ran (a cursor write, a read job, a positional call, a sync): once no task borrows the
handle it is dropped, and the last lease out closes the object, or handed back by `into_inner`,
which waits until no lease is out (a constant picks which, or neither).

The properties: `NoStaleRead` and `NoTornRead`, no read counts a value its cell no longer holds,
or a cell a write landed only part of, unless a positional write was out when it was taken (the
caller's own race, as with `pread` and `pwrite` from two threads); `AllLanded`, every cursor
write accepted is in the file at the end; for the append, `NoOverwrite`, no append writes over
what the file holds, and `CursorAtEnd`, the cursor is the file's end once an append landed;
`PositionalOrder`, each cell a positional write reached holds the write that came last in the
order the file keeps (a cursor write accepted after the positional one ended, else the positional
one, unless a cursor write raced it); `PositionalKeepsCursor`, an action property: no step of a
positional call moves the cursor or the epoch; `PosReadSees`, a positional read made after a
cursor write to its cell was accepted never reads what the cell held before it; `SyncCovers`,
every write a good sync covers (accepted, or returned for a positional one, before it was called)
is durable; `NoSyncAfterFailure`, no sync succeeds after one failed; `DirectExact`, on a direct
file every kernel read and write is exactly one a caller made; `NoCallOnClosed`, no job's call
runs on a descriptor that closed; and liveness (`Termination`).

52 configurations. 17 must pass: tasks that may go away and tasks that stay, a file opened to
append, positional calls, the generation, syncs that may fail and a direct file, each with its
safety properties; a handle dropped or handed back with cursor jobs, and with a positional write
left by its task, still out, with `NoCallOnClosed` beside them; and liveness for the cursor,
append, positional, generation, sync and direct scenarios. 23 mutations must break the property
their rule protects: a result that counts on the cursor alone breaks `NoStaleRead`, through the
seek back; a lent write that goes in flight over the one there, a lent write cancelled when its
task goes away, a write cut short on the ring that counts as done, and a write on the ring
cancelled with its shard that ends there break `AllLanded`; an append that lands where the cursor
was breaks `NoOverwrite`; a cursor that stays where an append was made breaks `CursorAtEnd`; a
positional write that goes while the cursor write before it is out (`pos_skips_wait`) and a
cursor write that goes while a positional write left by its task is out (`orphan_unordered`)
break `PositionalOrder`; a positional write that moves the epoch on breaks
`PositionalKeepsCursor`; a read job's result counted whatever positional write ended meanwhile
(`no_gen_check`), cells read ahead before a positional write ended handed out after it
(`ahead_no_gen`), and a positional write left by its task taken with the generation where it was
(`orphan_no_gen`) break `NoStaleRead`; a positional read that goes while the cursor write before
it is out breaks `PosReadSees`; a sync made with a write still in the write-behind or in flight
(`sync_no_flush`), and one made once the write-behind went out but ordered after the writes only
by the ring's drain (`sync_drain`: a write the shard handed to the blocking pool is not the
ring's, so the drain does not order it) break `SyncCovers`; a failed sync forgotten once reported
breaks `NoSyncAfterFailure`; a direct read through the read-ahead and a direct write that joins
the write-behind break `DirectExact`; a descriptor closed at the drop with a job out, and an
`into_inner` that does not wait for the jobs, each with a cursor job and with a positional write
left by its task, break `NoCallOnClosed`. 12 witnesses must be proved wrong: a read that reads
again, the lent write of a task that went away landing, a read after the seek that counts the
lent write's value, a write on the ring landing in two parts, one cut short whose shard went
finished on the pool, an append after the seek landing at the end, a read that reads again only
because a positional write ended, a buffered read asking the kernel for more than its caller (the
read-ahead the direct rule takes away), the positional write of a task that went away landing, a
sync that fails, and a cursor job and a positional write still out as their handle drops.

The model shows why a sync waits for the writes before it rather than relying on io_uring's
`IOSQE_IO_DRAIN` (`MF_sync_drain`): the drain orders an entry after the ring's earlier entries
only, and a write may be on the blocking pool (its shard went, or the handle's calls run there);
in Rust the drain would also hold the sync behind every other operation of the shard's ring, a
multishot receive among them, which may never end. Waiting costs nothing the drain would not:
the writes it waits for are the ones the sync must cover.

The Rust tests in `src/io/source/tests/owned.rs`, `src/fs/tests/owned.rs`,
`src/fs/tests/positional.rs`, `src/fs/tests/durable.rs` and `src/fs/tests/direct.rs` check the
same rules on real sockets and files, each with a mutation of the code that makes one of them
fail.

The zero-copy write (`LentZc.tla`): `TcpStream::write_zero_copy` (`src/net/stream.rs`,
`src/io/source/write/calls.rs`), its chunks (`lent_want`, `submit_ops` and `lent_back` in
`src/io/source/write/ops.rs`, `entry` in `src/io/source/ring.rs`), the buffer its sends share
(`src/io/source/write/pinned.rs`), the `Released` it hands back (`src/net/released.rs`), the
ring's side of it (`ShardRing::zero_copy` and `reapable` in `src/rt/shard/ring.rs`), and the
shard's drop (`src/rt/shard/going.rs`) with the ring's keeper (`hand_off` and `keep` in
`src/rt/shard/ring/keeper.rs`). A task writes a buffer of three units and then one of one unit,
each with zero-copy sends of at most two units, one chunk at a time, and then reads what its peer
wrote; the peer writes to the task before it reads anything. For each chunk the kernel takes what
the socket has room for (or fewer: a short count) and posts a first completion, then keeps reading
the task's memory until it lets go of every unit it took (once the peer read it, or earlier if the
network acknowledged it, which may never happen) and posts the chunk's notification, as separate
steps. The side counts the first completion and sends the next chunk from where the kernel
stopped; the write ends for the task once every unit was counted, so its next write may go out
while the first buffer is still pinned. The buffer comes back through its `Released` once the last
hold on it let go: the lent write's, and each send's until its notification. The task may drop a
`Released`, go away while its first buffer is lent, or see a send fail. The shard may go once both
writes ended, with notifications still out: its ring goes to a keeper, which applies the ring's
completions until none is out. Only a ring another thread can reap sends zero-copy; on any other
every chunk is copied, and lets go of the buffer at its completion. 27 configurations: 10 that
must pass (both ends writing before they read with the bytes fitting the socket; a socket of one
unit, so every chunk is cut short, with dropped `Released`s and failures; a task that may go away;
a shard that may go; and a ring only its own thread may reap; with `WireOrder`, the units in
order, each once, and a withdrawn write not at all; `Delivered`, the stream whole at the end, or
for a task that went the first buffer whole or not at all; `NoScribble`, no buffer handed back or
freed while the kernel may read it; and `Finishes`, every write ends and every buffer comes back,
for each), 9 mutations (a side that gets the first completion only with the notification, the bug
that shipped in `write_owned`, keeps the exchange from finishing; a shard that goes with no
keeper, its sends keeping what they hold for good (the bounded wait and forget the keeper
replaced), and a ring only its own thread may reap sending zero-copy, keep a `Released` from
resolving, breaking `Finishes`; a buffer handed back as the write ends, a dropped `Released` that
frees the buffer at once, and sends that let go of their buffers as the shard goes break
`NoScribble`; a short count sent again from the start, a chunk sent before the previous one
completed, and a started write withdrawn when its task goes break `WireOrder`), and 8 witnesses (a
write that pipelines behind a pinned buffer, a short count, a buffer freed at its last
notification after its `Released` was dropped, a withdrawn write, an abandoned write finished, a
send still holding its buffer as the shard went, a notification the keeper applied, a failed send
whose buffer waits for its notification).

`src/net/tests/zero_copy.rs` and `src/rt/shard/ring/tests_zc.rs` check the zero-copy write on real
sockets: both ends that write before they read finish, the buffer comes back whole only after the
notifications, a dropped `Released` and a dropped write, pipelined writes, a shard and a runtime
that go while a notification waits for a peer that reads only after (the buffer comes back once it
read, and nothing waits for it), and a ring only its own thread may reap copying.

## A datagram's sender

`sender` (`tla/sender/Sender.tla`) checks that a datagram receive names its sender
(`unix_addr`, `unnamed` and `recv_from_udp_uninit` in `src/io/sys/unix.rs`, used by the
receives of `src/io/source/read.rs` and `src/io/source/ring.rs`, and `poll_peek_sender` in
`src/net/udp.rs`). A peer sends datagrams; a receive takes one from the kernel and then makes the
sender's address for the task, while other code takes the process's last descriptors and gives
them back. Once the kernel handed a datagram over it is the receive's, so naming its sender must
not fail: the unnamed address of a socket pair's end is made from an empty path, with no socket;
and where macOS's kernel names no sender, for a receive into an empty buffer, a connected
socket's sender is its peer, whom the kernel names. A look at the next datagram's sender peeks
one byte, so the kernel names it on a socket connected to nothing too. 7 configurations: 3 that
must pass (`Conservation`, no datagram lost, `Order`, and `PeeksName`, every look at a sender
names one, with buffers that have room and with empty ones on a connected socket; and
`Termination`), 3 mutations (naming the sender by opening a socket, which fails with no
descriptor free, and answering an error for a receive into an empty buffer whose sender the kernel
did not name, both as the code once did, lose the datagram the kernel handed over and break
`Conservation`; a look that peeks into an empty buffer, as it once did, names no sender on a
socket connected to nothing and breaks `PeeksName`), and 1 witness (a receive names an unnamed
sender while no descriptor is free). The Rust test
`emfile::datagram_sockets_open_before_send_and_receive_with_no_descriptor_left` in
`tests/faults/emfile.rs` receives on a real pair with no descriptor left;
`a_datagrams_sender_is_peeked_and_it_stays_to_be_received` in `tests/net_api.rs` and the
`buffers_*` rows of `tests/sizes/datagrams.rs` peek a sender and receive into an empty buffer.

## A caller's own descriptor

`asyncfd` (`tla/asyncfd/AsyncFd.tla`) checks `AsyncFd` (`src/io/async_fd.rs`) and the side's
state for a caller's descriptor (`Kind::Fd`), at the grain of each atomic access. A peer writes;
on epoll and kqueue each write brings an edge that marks the side ready and counts it, and on
io_uring a poll armed while the side is not ready and a task waits completes once the socket
holds bytes. The task waits until the side is ready, notes the count (its guard's tick) and
makes its own call; a call that would block clears the side only if the count is still the
tick, since an edge after the call is the only one it gets. 5 configurations: 3 that must pass
(edges and the ring's poll with `AllTaken`, every byte taken, and edges with `Counted`), a
mutation that breaks `AllTaken` (a clear that does not look at the count), and a witness (a
clear that finds a newer edge and keeps the side ready). The unit tests in
`src/io/async_fd.rs` place an edge between the call and the clear on each side (the mutation
fails both), and `tests/async_fd.rs` reads every byte of a peer that writes while the caller
reads, on each backend.

The same family checks a socket's side turning from the handle's own IO to its caller's calls
(`try_io`, `async_io` on the sockets; `CallerMode.tla`, `CallerWrite.tla`). On the read side the
task reads once through the handle, which reads ahead and, on io_uring, keeps a multishot
receive out; then its own calls fail while the handle holds data read ahead (the handle's reads
take it first), stop a receive still out and wait for it to end, and from then on follow the
edges and polls as above. On the write side the bytes the handle wrote behind go out before a
send of the caller's. 11 more configurations: 6 that must pass (edges and the ring, four writes,
with `InOrder`, every byte taken once in the order written, and `AllTaken`; four bytes behind and
three of the task's own with `OutOrder` and `AllOut`), 3 mutations (a call with data read ahead
breaks `InOrder`; a stopped receive that ends with the side ready and wakes nobody breaks
`AllTaken`; a send with bytes still behind breaks `OutOrder`), and 2 witnesses (a call that finds
data read ahead, a stop that ends the receive with nothing brought). `tests/net_caller.rs` runs
each case on both Linux backends, and the three code mutations fail it.

## A source's operations on its shard's ring

`ring` (`tla/ring/RingOp.tla`) checks how a side of a source works on an io_uring ring
(`src/io/source/ring.rs`, `src/io/source/write/ops.rs`, `src/rt/shard/ring.rs`) when the
side is held from another thread than the ring's. A task there writes twice; each write is
accepted into the side, and the side's operation goes to the ring one at a time. Only the
ring's own thread submits: a task elsewhere posts a kick once it has let go of the side, and
the ring's thread submits what the side wants. Submitting raises the side's flag (an operation
is out) and the source's count of operations in the kernel. When the kernel completes one, the
ring's thread lowers the count, leaves the completion in the side's one-slot mailbox and
defers it to the side: applied at once if the side is free, else by its holder as it lets go,
which then asks for the next operation. (`Source::op_done` in `src/io/source/ops.rs` tries the
write side's hold first and applies a completion to a free side with no trip through the
mailbox, which is the model's put and apply with nothing between them; a held side takes the
mailbox and the deferred bit, as modelled.) A retire deregisters the source, cancels what is out
and waits until the count is zero: it puts its waker in, raises a flag, and only then looks at
the count, while the ring's thread lowers the count and then looks at the flag. 11
configurations: 3 that must pass (`OneOut` and `NoOverwrite`, one operation out and one
completion in the mailbox at most; `SingleIssuer`, only the ring's thread submits;
`RetiredIdle`, once the retire returned the kernel holds no operation of the source; the
retire returns; and with no retire, both writes go out), 6 mutations (a second operation out
while one is breaks `OneOut`; a task that submits from its own thread breaks `SingleIssuer`;
a want left unkicked keeps the writes from going out; a completion that wakes nobody, and a
retire that looks at the count before raising its flag, keep the retire from returning; a
retire that does not wait breaks `RetiredIdle`), and 2 witnesses (the task's thread applies a
completion as it lets go; the retire waits for one).

`tla/ring/RingLent.tla` checks `write_owned` through the ring when its task may go away while
the buffer is out with the kernel: the kernel takes some of the buffer, or, if a cancel came
before it took any, completes cancelled; the task's going asks for that cancel when nothing
went out yet; a completion that leaves an abandoned buffer with nothing sent withdraws it, and
one that leaves part of it sent goes on to the end. 6 configurations: 2 that must pass
(`WholeOrNothing`, the stream gets the buffer whole or not at all; and the write ends), 2
mutations (withdrawing a write part of which went out breaks `WholeOrNothing`; leaving the rest
of an abandoned write unsent keeps it from ending), and 2 witnesses (a withdrawn write; an
abandoned write finished after part of it went out).

`tla/ring/RingMulti.tla` checks a stream's read side with multishot receives
(`src/io/source/read/ops.rs`, `src/io/source/read.rs`, `src/rt/shard/ring.rs`): the reader's
first read arms one receive, the kernel fills a buffer of the shard's group per message and
posts an arrival, the ring's thread hands each arrival to the side in order and stops the
receive past its cap, and the receive ends (cancelled, dry, at the stream's end) with one last
completion the side applies after its arrivals. A reader of lent buffers stops an armed receive
and reads straight into its buffer. A read into a buffer of the caller's of at least 16 KiB
(`ReadState::read_direct`) is made by the task with `read(2)` when nothing is read ahead and no
receive is out; with a multishot receive out it stops it through the same `stop_multi` and
waits, so the stop's last arrivals and its end come first, and the next small read arms a
receive again. 17 configurations: 6 that must pass (four messages through three buffers with a
cap and a burst of one, plain, with lent buffers and with reads big and small, with `InOrder`,
no message lost, doubled or reordered, `OneOut`, `NoArmWithAhead`, `CapHeld` and `BuffersBack`,
every buffer back in the group at the end; and `Termination` for each), 5 mutations (a ring that
never stops a receive breaks `CapHeld`; a receive armed while one is out, and a small read that
arms one while a big read's stop is in flight, break `OneOut`; a big read straight on the
socket while the receive is out, or with chunks read ahead, breaks `InOrder`), and 6 witnesses
(a dry end, a stop past the cap, a message into a lent buffer, a message read straight on the
socket, a big read's stop, and a receive armed again after it). The reader makes at most two
big reads, so a reader that keeps switching between sizes cannot spin the model.

`tla/ring/RingInc.tla` checks the same receives on a group filled in pieces (`IOU_PBUF_RING_INC`;
`RecvGroup::hand_out`, `settle`, `take_back` and `charge` in `src/rt/shard/recv.rs`, `arrived` in
`src/rt/shard/ring.rs`). The kernel keeps a buffer while it has room: each message goes after the
bytes already there, and its completion carries BUF_MORE until the one that fills the buffer; a
receive that ends leaves the buffer partly filled for the next. For each buffer the ring's thread
counts where the next piece starts, the chunks of it out and whether the kernel is done with it,
and gives it back to the kernel only when the kernel is done and no chunk of it is out. A chunk
counts at least a minimum against its receive's cap, and a datagram socket takes pool buffers
while the group is filled in pieces. The model keeps the message each unit of a buffer holds, so
a buffer filled again under a live chunk shows as a wrong message read. 13 configurations: 6 that
must pass (four and five messages, with one and two completions posted ahead of the ring, and a
datagram socket, with `InOrder`, `NoReuse`, no buffer back under the kernel or a live chunk and
never twice in the kernel's ring, `BuffersBack`, `CapHeld`, the chunks of a receive within its cap
at the minimum charge plus the batch the kernel posted when the ring passed it and the cancel's
burst, and `NoCut`, no datagram cut; and `Termination` for each), 5 mutations (a buffer back on
its last chunk while the kernel still holds it, and back on the kernel's last piece while chunks
are out, break `NoReuse`, and with six messages the first loses a message, breaking `InOrder`; a
chunk that counts its length only lets a peer sending a unit at a time past the cap, breaking
`CapHeld`; a datagram socket arming a multishot receive on the group gets a datagram cut, breaking
`NoCut`), and 2 witnesses (a buffer handed out in two pieces, a buffer given back).

`tla/ring/RingClose.tla` checks a ring source dropped with an operation out
(`Source::detach_ring` in `src/io/source/ring.rs`, `ShardRing::detach`, `take_out` and
`finish_leaving` in `src/rt/shard/ring.rs`, `RingPoller::submit_now`, the close's lease in
`src/io/source/requests.rs`): a listener with an accept out, its slot a fixed file of the ring's
table or one naming its own descriptor, which the ring never copies. The detach cancels what is
out, hands every entry naming the slot to the kernel at once (the cancel ending the accept in the
same call, the kernel's task work run), takes the file out of the table and releases the slot,
which stays reserved until the accept's end is taken; a lease keeps the handle's object open
until no entry naming its number waits to reach the kernel: where the kernel holds completions it
has no room for, until the ring's next wait, and for a drop on another thread, until the ring's
thread applies the detach from its mailbox. Someone opening a file meanwhile gets the number once
the object closed. 14 configurations: 8 that must pass (each kind of slot, dropped on the ring's
thread or another, with the kernel taking everything or busy, with `NoStray`, no entry reaches
the kernel on a number another file took; `SlotReserved`, a slot is free only once no entry names
it; `ClosedAtDrop`, dropped on the ring's thread with the kernel taking everything, nothing holds
the file when the drop returns; and liveness, `AllClosed` and `Termination`), 4 mutations (no
lease, so the object closes with an entry naming its number still queued, breaks `NoStray`; the
cancel left to the ring's next wait, and the file kept in the table until the slot frees, the two
halves of the bug a listener dropped with an accept out still accepting after, break
`ClosedAtDrop`; a slot free at its release breaks `SlotReserved`), and 2 witnesses (a detach left
to the next wait; the number taken by another file). `tests/faults/drops.rs` drops a listener
with an accept out and checks a connect is refused when the drop returns, and drops a stream with
a read out from a thread that then blocks and checks its peer reads the end.

`tla/ring/RingSend.tla` checks a datagram socket's sends (`WriteState::send_now` in
`src/io/source/write.rs`, the poll it waits on in `submit_ops` and its end in `op_done`,
`src/io/source/write/ops.rs`; on epoll and kqueue the socket's edge in `serve` stands where the
poll does). A task sends datagrams one call after another; the first may be its own buffer lent
to a send of the ring's, whose task may go away while it is out. Every other call hands its
datagram to the kernel itself once nothing of the side is out, and the kernel answers at once:
taken, refused (too large for any socket), or no room, when the call waits for the poll and
tries again. A peer reads what the socket holds, which starts full. Before, a call copied its
datagram and queued it for the ring, answering taken at once: a datagram too large for the
kernel was reported sent and its refusal reached the next call, which the size sweep
(`tests/sizes.rs`) found at 65 508 bytes of UDP. 10 configurations: 4 that must pass (a lent
datagram whose task may go, then a refused one and one more; borrowed sends only; with
`OwnResult`, every call answers the kernel's own verdict on its datagram, and `InOrder`, the
kernel takes the task's datagrams in the order it sent them; and `Termination` with
`Delivered`, every call ends and every datagram the kernel can take reaches the peer), 3
mutations (the old queue-and-answer breaks `OwnResult`; a call that goes to the kernel with the
lent send of a task that went still out overtakes it, breaking `InOrder`; a poll that wakes
nobody breaks `Termination`), and 3 witnesses (a call that waits for room, a refusal answered by
its own call, a call that waits behind the lent send of a task that went).

`tla/ring/RingRecv.tla` checks a datagram socket's receives (`receive_now` and
`datagram_ready` in `src/io/source/read.rs`, the bound in `datagram_want` and the poll's end in
`op_done`, `src/io/source/read/ops.rs`, the edge in `serve`), on epoll or kqueue and on a ring.
A peer sends a small datagram and then the largest the socket carries, which for a Unix socket
is more than the pool's largest buffer holds; the task receives each into a buffer that holds
it, maybe asking first whether the socket is readable. The task receives a datagram itself into
its own buffer (on epoll and kqueue always, on a ring a Unix datagram), waiting for an edge or
the ring's poll when none is there; a look at readability looks and takes nothing. Only a ring's
UDP socket has the ring receive ahead, into a buffer that holds the largest datagram UDP
carries. Before, the side received ahead into the pool, into a buffer as large as the task's
(failing with `OutOfMemory` for a task's buffer the pool could not hold) or, for a look at
readability, into the pool's largest buffer, which cut a Unix datagram past 64 KiB that the
task's buffer would have held whole: both found by the size sweep. 15 configurations: 8 that
must pass (Unix and UDP datagrams on epoll and on a ring, with `NoCut`, every datagram received
whole, and `InOrder`; and `Termination` for each), 5 mutations (a look that receives ahead into
the pool's largest buffer, a ring's UDP receive ahead into a buffer below the largest datagram,
and a ring that receives a Unix datagram ahead for a task that receives it itself break `NoCut`;
a poll, and an edge, that wake nobody break `Termination`), and 2 witnesses (a call that waits
for the poll, a datagram received ahead and taken).

`tla/ring/RingStarve.tla` checks stream reads and replies that share one shard's pool of IO
buffers on a ring (`submit_ops` and `op_done` in `src/io/source/read/ops.rs`, `read_direct` and
`reads_itself` in `src/io/source/read.rs`, `write`, `write_direct` and `writes_itself` in
`src/io/source/write.rs`; a ring write goes direct at every size, `WRITE_RING` of
`src/io/source/direct.rs` being 0). Three connections over a pool of two buffers, two of whose peers never
send: with no receive group to draw from (none configured, or the group dry) a read takes a buffer
of the pool for a receive that waits in the kernel, and holds it while its peer stays idle. Before,
a reader or a writer that found the pool empty waited for a buffer to come back and armed nothing,
so a request that came was never read, or its reply never sent, while both buffers waited for
peers that never send. Now it asks for a poll that holds no buffer, and once the socket has bytes,
or room, it reads them into its own buffer or writes from its own. 8 configurations: 3 that must
pass (`Buffers`, every buffer free, held by a receive out, a request read ahead or a reply not sent
yet; and `Served`, every request sent is read and answered, with two idle peers and with none), 3
mutations (a starved reader that waits for a buffer to come back, a starved writer that does, and a
poll whose end wakes nobody break `Served`), and 2 witnesses (a reader finds the pool empty; a
starved side uses its own buffer). `tests/idle_peers.rs` fills the pool of a 512 KiB cap with
receives of idle peers and checks every other peer is answered.

`lean/Nuclei/Files.lean` checks the ring's file slots (`src/syscore/linux/iouring/files.rs`,
`take_out` in `src/rt/shard/ring.rs`): every entry names its source's file by a slot of the
kernel's fixed-file table, or past a full table by a slot naming the source's own descriptor (the
ring makes no copy), and counts on that slot from its submission to its completion; a source that
goes takes its file out of the table at once. For every sequence of operations from a fresh table
it proves that a free slot is held by nobody, named by no entry and holds no file (`inv_run`, for
a table of any size, as the process's descriptor limit sizes it); that a slot a source takes has
no entry naming it, so an entry on its way never reaches the next source's file
(`taken_slot_is_unnamed`); that a free slot is never freed again (`no_double_free`); that a slot an
entry names is never free, so it stays reserved after its source went (`named_slot_is_not_free`);
and that a release takes the file out of the table whatever entries still name the slot
(`release_takes_the_file_out`); and it computes the worked example of a slot released with a read
out and handed out again only once the read completed (`example_reuse_waits`). The Rust tests in
`src/rt/shard/ring/tests.rs` run sources past a full table, a retire with a read out and a
shard that goes with one, each with a mutation of the code that makes one of them fail.

## A reader the pool has no buffer for

`starve` (`tla/starve/FdStarve.tla`) checks the rule of `tla/ring/RingStarve.tla` on a readiness
poller, epoll or kqueue (`serve`, `leaves_to_tasks`, `complete_fd`, `read_ahead`, `read`,
`reads_itself`, `read_direct`, `look_again` and `unstarve` in `src/io/source/read.rs`). There no
receive waits in the kernel: a read too small to go straight into the task's buffer has the
driver read ahead into a buffer of the pool, and a task that peeks and then stops reading keeps
that buffer for as long as it waits. Two connections over a pool of one buffer, one of whose
tasks peeks and stops. Before, a reader that found the pool empty waited for a buffer to come
back, and none ever did. Now a starved reader reads, or peeks, straight from the socket into its
own buffer; one that finds the socket empty waits for the next edge, which wakes it to read
itself, and one that just found the pool empty while the socket may hold bytes no edge will
report again looks at once. A peek leaves the bytes in the socket, so it never marks the socket
drained. A TCP read on Linux goes straight into the task's buffer at every size (`inq_wanted`,
`read_direct`, `sys::read_inq`): it can fill its buffer exactly, so the kernel's count of what a
read leaves (`TCP_INQ`, which counts a pending end of stream as one) says whether the socket is
empty, and a count of none lets the next read wait for the edge. The model's `Exact` runs it on a
request of two pieces, one read each. 11 configurations: 4 that must pass (`Buffers`, every buffer free or holding a request
read ahead; and `Read`, every task that reads gets its request, with a task that stops, with
none, and with exact reads), 5 mutations (a starved reader that waits for a buffer to come back, an edge that starves
the side and wakes nobody, a reader that waits for the next edge after the pool turned it away,
a peek taken as having emptied the socket, and an exact read taken as having emptied it whatever
the kernel counts, break `Read`), and 2 witnesses (a reader finds the
pool empty; a starved reader calls on the socket itself). The second test of
`tests/idle_peers.rs` fills a pool of a 64 KiB cap with bytes read ahead for tasks that peeked
and stopped, on every backend, and checks every other peer is answered. A pipe's write the pool
has no buffer for goes straight from the task's buffer too (`writes_bytes` in
`src/io/source/write.rs`), on epoll and kqueue as on a ring, as a socket's always does there; the
pipe test of `tests/idle_peers.rs` checks it on every backend.

## A listener's stream of connections

`accept` (`tla/accept/AcceptStream.tla`) checks the stream `TcpListener::accept_multi` gives
(`TcpStreamGenerator::poll_next` in `src/net/tcp.rs`, `out_of_descriptors` in
`src/rt/counters.rs`), and the accept under it on a readiness poller (`ReadState::accept_ahead`
and `accept_checked` in `src/io/source/read.rs`), when the process runs out of descriptors.
Peers connect into the backlog; the rest of the process may take every free descriptor for a
while and gives them back in the end. A poll asks the listener for the next connection: one
there and a descriptor free, it is accepted and the stream's pause starts over at its least; out
of descriptors (EMFILE or ENFILE), the stream waits on a timer for its pause, which then doubles
up to its most, and asks again when the timer fires; any other error ends the stream. On macOS an
accept that fails for want of a descriptor closes the connection it took off the backlog
(`Drops`), so after one the side first copies the listener's descriptor and closes the copy: none
free, it answers EMFILE and takes nothing; one free, it accepts, though the rest of the process
may take the last descriptor in between. The side hands an error over once and the connections
queued before it first, so a retry asks the listener afresh. Time is the ticks left on the timer.
11 configurations: 4 that must pass (two and three connections across two outages, with
`NoBusyLoop`, at most one EMFILE between two ticks, `FirstWaitLeast`, the first pause after an
accept the least, `LostPerOutage`, no more connections closed than outages began, and
`AllServed`, every connection accepted once descriptors are free again or closed by a failed
accept; the same on macOS, where a failed accept closes the connection it took; and an outage
followed by another error of the listener, with `OtherEnds`, the stream ends, and
`NothingAfterEnd`), 5 mutations (the stream that ended on EMFILE, as it did before, breaks
`AllServed`; a retry with no timer breaks `NoBusyLoop`; a pause that does not start over after an
accept breaks `FirstWaitLeast`; on macOS a retry with no check for a free descriptor, as before,
and a failed accept that does not mark the side for that check, break `LostPerOutage`), and 2
witnesses (a retry after a pause that accepts a connection; a check that found a descriptor free
and an accept that still closed a connection, the race the check cannot close).

## A stream's close

`shutdown` (`tla/shutdown/Shutdown.tla`) checks a stream's close (`WriteState::close` in
`src/io/source/write.rs`, which flushes and then calls `shutdown_write` of `src/io/sys/`; the
driver's writes of the write-behind are `WriteState::push_fd`). A task writes bytes, which wait in
the write-behind until the driver hands them to the kernel, one per call; then it closes the
writing half again and again. A close waits until the write-behind is out, then asks the kernel to
shut the writing half; the first shutdown puts the end of the stream on the wire. The kernel
answers a shutdown of a half shut already its own way: Linux says it went, macOS says ENOTCONN. 5
configurations: 2 that must pass (two bytes and three closes on Linux and on macOS, with
`AsTheKernel`, every close answers what the kernel answered to that shutdown, which is std's
answer; `FinLast`, the end of the stream follows every byte written before the close, in order;
`NoneLost`, no byte is refused by a half shut under it; and `Termination`), 2 mutations (a close
after the first that answers Ok from the side's own note that it shut, as the code did, breaks
`AsTheKernel` on macOS; a close that shuts before the write-behind is out breaks `FinLast`), and 1
witness (a second close on macOS answered ENOTCONN). The Rust test
`half_close::*::our_shutdown_delivers_every_byte_and_reads_go_on` in `tests/faults/half_close.rs`
compares a second close with std's on a real stream.

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

## A ring source's attach from another thread

`attach` (`tla/attach/Attach.tla`) checks the attach of a source to a ring performed off the
ring's thread, racing a close or an evict (`src/io/source/requests.rs`,
`src/io/source/ring.rs`). The performing thread marks the registration under way and posts the
ring a lease on the handle's object, which keeps the descriptor open and its number the
handle's until the ring's thread is done with the attach (no copy of it is made); the ring's
thread puts the file into its table and hands the slot back as a request, which whoever performs
it (both sides held) keeps if the attach is still wanted, or lets go. After the evict the source
may attach to another shard under a token of the same number (every shard numbers its own), so
the ring's look and the install's taking check the registration's shard with its token. 13
configurations: 5 that must pass (a close, an evict, both, liveness, and an evict followed by an
attach to another shard), 4 mutations (2 that leak the file into the table of a closed source,
`NoLeak`; a look and a taking by the token alone, which hand the source registered with the other
shard a slot of this ring's table, `NoForeignSlot`; an attach with no lease, closed between the
ring's look and its install, which puts a closed number's file into the table,
`NoClosedInstall`), and 4 witnesses.

`tla/attach/Route.tla`, in the same family, checks a read of a ring source from a thread that
runs no shard while the source's registration finishes (`Source::with_io`, `ensure_registered`,
`ops_registered`, the side's `submit_wanted`). `with_io` reads the route before the side is
taken: NotYet while the registration is under way, which issues nothing, Elsewhere once it is in
place, which kicks the ring's thread. The registration is done with the sides held on the ring's
thread, where `ops_registered` submits for a waiter it finds, or by the side's holder as it lets
go. After its step, a read whose route was NotYet looks once more and, finding the source
registered, asks the side to submit through the same path (the fix of commit 4bc2518): without
it a registration that lands between the route and the step finds no waiter, the step leaves one
and asks with NotYet, and nothing ever submits. 6 configurations: 4 that must pass (the read
during the registration and after it, each with `OneSubmit`, one submit for the waiter, and no
deadlock, and with `Served`, a waiter's operation is submitted in the end, and `Termination`), 1
mutation (no look after the step breaks `Served`), and 1 witness (the late look asks for the
submit).

## A shard that goes

`going` (`tla/going/Going.tla`) checks how a shard that goes waits for the evictions of its
sources (`src/rt/shard/going.rs`, `src/io/source.rs`, `src/io/source/requests.rs`) before it
looks at what its ring has out. The shard evicts a source with a receive out on its ring, at
once or, while another thread holds a side, by that holder as it lets go, and waits until the
source is no longer registered with it; then its mailbox, where the eviction left the cancel,
ends the receive. A task on another shard registers the source as soon as the eviction is done,
under a token of the same number, and its receive goes out once the old one has ended. The
wait checks the registration's shard with its token, and every end of a registration (a failed
ring attach too) clears the shard. 9 configurations: 4 that must pass (the shard's own
eviction, a holder's, a registration that was a failed attach, and that with a holder and the
other shard), with nothing left out on the ring once the shard has gone and everything ending;
3 mutations (a wait by the token alone, which waits for the other shard's registration while
that one's receive waits for this shard's cancel, `AllEnd`; a failed attach that keeps its
shard, `AllEnd`; no wait, which lands the holder's cancel after the shard has gone,
`NoStrandedOp`); and 2 witnesses (the other shard's registration under the same number while
the shard waits; a holder's eviction).

`Behind` (`tla/going/Behind.tla`) checks the bytes a source's writes took into its write-behind
when the shard that drove them goes (`Shard::leave` and `drain_unsent` in
`src/rt/shard/going.rs`, `Source::poll_drain`, `WriteState::submit_ops` and `op_done`,
`attach_port` and `waits_for_ops`): no task of the handle's may poll that side again, so the
shard hands a source that still holds bytes to a drain task on a worker. The task looks each
time it is woken: with nothing left it ends; registered nowhere with nothing out, it registers
the source with its worker, whose driver writes the bytes out; with an operation still out (a
port's TCP send, which the eviction never cancels, or a ring's send it cancels, which may come
back undone) it waits for that operation's end, which wakes it. 7 configurations: 3 that must
pass (a readiness poller with every byte behind, a port with a send out, a ring with a send
cancelled), with `Conserved`, no byte lost or sent twice, `AllSent`, every byte reaches the
kernel, and `DrainEnds`; 3 mutations (no drain task, as before, which left the peer waiting for
good, `AllSent`; a drain task that does not register the source, `AllSent`; an operation's end
that wakes nobody, `AllSent`); and 1 witness (the drain task moves the source to the worker).

`Linger` (`tla/going/Linger.tla`) checks what a dropped handle leaves when its runtime drops
(`src/rt/linger.rs`, `Runtime::drop` and `finish_lingering` in `src/rt/runtime.rs`, `Counted`'s
drop in `src/rt/sched.rs`, `Shared::settled` and `drop_stopped` in `src/rt/shard.rs`, the file
write's count in `src/fs/call.rs`, the handle's drop, a gone shard's sources): a worker's task
holds a handle whose write-behind holds bytes, and whose file has one write out. A handle dropped
while the runtime runs hands its linger to a worker; once the runtime's drop began, to the
runtime's list. The drop stops the worker, whose shard drops its tasks: the handle's (its linger
to the list), unless that task waits on the write, which then drops it where it ends (a ring's
keeper, the blocking pool): its wake lands in the stopped shard's inbox, and the thread that drains
it (the write's own, or another that held the drain) drops the task inside its runtime still. A
linger's own task not done yet hands what it left back (`Guarded`): at once if the stop wakes it,
or, held by the write, once the write ends. The write counts itself on the worker's shard until
its end's wake went. The drop runs the list on its own thread until no linger it counts is left,
no write is counted on a worker and no worker's stopped inbox is still to be drained, then
returns; one handed after runs on a thread of its own, with a runtime of its own. A background
shutdown waits for nothing, and its lingers all run on threads of their own. A shard whose thread
is blocked in a task's poll after the shard was handed on drops what its inbox holds only once
that poll returns, which the drop does not wait for. 17 configurations: 3 that must pass
(`DropWaits`, the drop returns only once every byte reached the peer, `NoDefault`, no runtime is
started for a linger, and `TypeOK`; liveness, `AllSent` and `Termination`; and a background
shutdown, with `NoDefault`, `AllSent` and `Termination`), 9 mutations (a drop that does not run its list; one that runs only what its list
holds as it looks (the fix's first form, which left a file's last writes unlanded); a linger's
task lost as its worker stops; a linger handed to the stopping shard of its thread, which never
runs it; the held task dropped outside its runtime, its linger to a thread of its own; a write
counted out before its wake went; a drop that does not wait for its workers' writes; and one that
does not wait for a stopped inbox to be drained: all break `DropWaits`; a linger handed to the
process default runtime, which starts for it, breaks `NoDefault`), and 5 witnesses (a linger's
task handed back as its worker stops; one held by the write and handed back later; the drop
running a linger of its list; the task with the handle held by the write as its worker stops;
another thread draining its wake after the write counted itself out).

## A blocking job of a Windows handle

`job` (`tla/job/Job.tla`) checks the cancel of a pool thread's call on a Windows handle opened
for synchronous IO (`src/io/source/port.rs`, `Job`): `CancelSynchronousIo` ends whatever call
the thread is in, so a cancel must never land after the job's call, in a later call of the
same thread, and must never be lost. 8 configurations: 2 that must pass, 3 mutations (no
CANCELLING flag lets a cancel stray into a later call; no retry deadlocks; no OUT flag breaks
termination), and 3 witnesses.

## A stopped shard's completion port

`port` (`tla/port/PortKeep.tla`) checks how a Windows shard that stops running hands its
completion port to a keeper, and how a source moves off that port while operations are still
out (`src/rt/shard/port.rs`, `src/rt/park/iocp.rs`, `src/io/source/port.rs`). The shard
waits for every eviction a side's holder performs, delivers what the port has ready, and
hands the rest to a keeper job, which drains the port until nothing is out or the shard runs
again; the shard calls off a keeper that has not started, and stops and waits for one that
keeps, so one thread at most drains the port. A source's registration on another port waits
while its handle has operations out (Windows cannot move it then) and is made again as the
last one ends: the registration raises a flag and looks at the count again, the last
completion lowers the count and looks at the flag. 13 configurations: 3 that must pass (one
drainer and no failed registration, every packet delivered and the source registered,
whether the shard runs again or not), 6 mutations (no keeper and no wait for the evictions
strand packets, `AllDelivered`; no wait for the keeper and a keeper that ignores being called
off break `OneDrainer`; no look at the count after the flag breaks `Registers`; a bind failure
that fails the registration breaks `NoFailure`), and 4 witnesses.

## Idle peers on a completion port

`tla/port/PortStarve.tla` checks stream reads and replies that share one shard's pool of IO
buffers on a completion port, over TCP, over UDP and over pipes (`submit_ops`, `submit_wanted`
and `op_done` in `src/io/source/read/ops.rs`; the zero-byte receive, its look on a datagram
socket and the room read in `start`, `stream_memory` and `outcome` of `src/io/source/port.rs`;
`read_starved` and `receive_starved` in `src/io/source/read.rs`; `write_room` and `send` in
`src/io/source/write.rs` and the room send's end in `src/io/source/write/ops.rs`; and `buffer`
and `unstarve` of each side). Three connections over a pool of two buffers, each task reading a
request, writing a reply and asking for a next request its peer never sends; some peers never
send at all, and some tasks peek at their request and never read it, so its buffer stays read
ahead. Receives of idle peers and bytes no task reads may hold every buffer. Before, a reader or
a writer the pool had no buffer for waited for one to come back, which none did, and a request
that came was never read or answered (`tests/idle_peers.rs` on Windows: 256 receives of idle
peers held the whole 1 MiB cap, and the reads of the peers that sent never went out). Now a
socket's read the pool has no buffer for sends a zero-byte receive, which holds none; on UDP it
looks (`MSG_PEEK`), since a datagram received into no room is cut to nothing and the rest lost
(WSARecvFrom's remarks), as libuv's zero reads of UDP do (`src/win/udp.c`). Its end sends a
receive into a buffer at once, which ends at once since the bytes are there, or, with the pool
still empty, wakes the task, which reads the bytes, or receives the datagram, straight from the
socket into its own buffer (the zero-byte receive said they are there, so the call on the
blocking socket never waits). A pipe's read the pool has no buffer for reads into its slot's own
room, a few hundred bytes, and its task takes them from there. A writer the pool has no buffer
for sends from its slot's room, or a datagram straight from its own buffer, with nothing of the
socket's in the kernel ahead of it. A read or a write the pool has a buffer for is as before. 27
configurations: 12 that must pass (`Buffers`, every buffer free, held by a receive out, a request
read ahead or a reply not sent yet, and `NeverBlocks`, no reader's own call meets an empty
socket; and `Served`, every request sent is read and answered but those of tasks that stopped;
over TCP with two idle peers, with none and with two tasks that stopped, and over UDP and pipes
with idle peers and with stopped tasks), 11 mutations (a zero-byte receive whose end sends
nothing and wakes nobody, a buffer that comes back and wakes no starved side, a starved reader
that waits for a buffer, as before, with idle peers and with stopped tasks, a starved writer that
does, on TCP and on pipes, a zero-byte receive that ends with the pool empty and wakes nobody,
and a pipe's read that waits for a buffer, as before, break `Served`; a receive that leaves its
side marked readable, a starved reader that calls while its zero-byte receive is out, and a
datagram socket's zero-byte receive that does not look break `NeverBlocks`), and 4 witnesses (a
side finds the pool empty; a read waits in a zero-byte receive; a starved side uses its own
memory; a pipe's read waits in its slot's room).

## Signals

`signal` (`tla/signal/Signal.tla`) checks how a unix signal's deliveries reach a listener
(`src/signal/unix.rs`, `src/signal/mod.rs`). The handler raises the signal's count, then
writes to the process's wake descriptor; each shard's pump drains the descriptor, then
dispatches: it loads the count, swaps it into the count the last dispatch woke for, and wakes
the listeners if it moved. A listener takes an item when the count moved since it last looked,
else registers its waker and looks again before it sleeps. 7 configurations: 2 that must pass
(never an item more than the deliveries; every delivery seen), 3 mutations that lose a
delivery (`SeesAll`: the handler writing before it counts, a pump looking before it drains, a
listener sleeping without its second look), and 2 witnesses (deliveries coalesced into one
item, a pump's wake).

## Child processes

`process` (`tla/process/ExitWatch.tla`, `tla/process/KillReap.tla`) checks how a child's exit
is watched and reaped (`src/process/exit.rs`, `src/rt/shard/children.rs`,
`src/rt/shard/ring/exits.rs`). `ExitWatch`: one shard at a time watches a child; a ring's
watch is a `WAITID` that reaps the child itself, so a second watch would find ECHILD. A shard
that stops running cancels its watch and lets the child go only at that watch's completion; a
waiter registers its waker, has its shard watch the child unless one does (a `try_wait` before
may have, with no waker), and looks again before it sleeps; a dropped `Child`'s child is
watched elsewhere. `KillReap`: a kill by pid and a reap by pid exclude each other through the
child's state, so no signal goes by pid after the reap. 12 configurations: 4 that must pass, 4
mutations (letting go at the cancel breaks `OneWatch`; no second look and an unwatched orphan
break `Settles`; a killer that only looks breaks `NoKillAfterReap`), and 4 witnesses.

## The broadcast channel

`broadcast` (`tla/broadcast/Broadcast.tla`) checks the broadcast channel's senders and a
receiver (`src/sync/broadcast.rs`). A sender takes the next position from the tail and
publishes it into the position's slot unless the slot holds a later position (a send a ring
later got there first: its value goes, every receiver is past it); the last sender to go
closes the channel. A weak sender's upgrade counts one more sender only while one is left (a
compare-and-swap on the count), and the sender it makes sends and goes as any does. The
receiver looks at its next position's slot: its own position is a value taken, a later one is
a lag (it goes on from the tail, read then, less the ring's size, counting what it skipped),
anything else is nothing yet, unless the channel is closed and its next position is the tail.
11 configurations: 3 that must pass, with a weak sender upgrading at any point (two senders and
the weak one, three values each: what was taken plus what was reported lost is every value sent,
each state checked once up to the two senders' names; the receiver reaches the close, checked
over every behaviour in two smaller races, the three senders with one value each, and one sender
and the weak one with three values each), 4 mutations
(a send publishing over a later position leaves the receiver waiting for good, `Finishes`;
stopping at the close with values in the ring, an uncounted lag, and an upgrade that makes a
sender after the last one went, whose values come after the receiver saw the close, break
`Counted`), and 4 witnesses (a lag; a send that finds a later position in its slot; an upgrade
that makes a sender; one that finds none left). `lean/Nuclei/Broadcast.lean` proves the
arithmetic, for every size: a later position found in a receiver's slot is at least a whole
ring later, so the receiver's new position is past its old one; the ring's last positions sit
in different slots; and a take or a lag keeps taken plus lost equal to the positions passed,
so at the close a receiver accounts for every value sent after it subscribed.

## The watch channel

`watch` (`tla/watch/Watch.tla`) checks the watch channel (`src/sync/watch.rs`) at the grain of
its atomic steps: a send's store of the value, its version bump and its wake; a modifying send's
load, its compare-and-swap and its retry; the last sender's close and wake; `changed` making its
`Notified`, reading the state word and deciding; a borrow's read of the version and of the value;
the receivers' count and its wake; and a sender's `closed` waiting for the receivers to go. A
`Notified` is abstracted to what `Notify` promises: it hears every wake made after it was made. 14
configurations: 6 that must pass (sends, and sends mixed with modifying sends, each run once with
the receiver that waits for changes and once with the borrower, as the two decide nothing by each
other; modifying sends, where no change is lost; a sender waiting for the receivers to go, which
ends), with `changed` never answering closed
while a version it has not seen is there (`ErrOnlyWhenSeen`), a borrow never pairing a version
with an older value (`PairedNoOlder`), the version counting every publication, the receiver
seeing the last version and ending; 5 mutations (a modifying send that stores without comparing
loses a change, `NoLostModify`; `changed` reading the state before making its `Notified` misses a
wake and never ends, `ReceiverEnds`; answering the closed bit first misses the last change,
`ErrOnlyWhenSeen`; a borrow that loads the value before the version pairs it with a newer
version, `PairedNoOlder`; a `closed` that counts the receivers before making its `Notified` never
ends, `ProducerEnds`); and 3 witnesses (a change seen; a compare-and-swap that lost and started
over; a borrow whose value is one send ahead of its version, the harmless extra change the docs of
`borrow` warn of). `lean/Nuclei/Watch.lean` proves the state word's arithmetic for every word: a
send keeps the closed bit and moves the version one step, wrapping; a close keeps the version;
after any number of sends short of a whole turn of the word (2^63) the version differs from the
one before them, so a receiver that slept through them hears of a change; and `mark_changed`
always makes a change.

## The cancellation token's tree

`cancel` (`tla/cancel/Cancel.tla`) checks the tree behind `CancellationToken`
(`src/sync/cancellation.rs`, `node.rs`) at the grain of its atomic steps: the making of a child
node, its insert into its parent's registry and its read of the parent's flag; a cancel's flag
set and its walk, reading each registry with an upgrade of each weak handle, marking what it
found and going into all of it; and each strong handle let go, with the free, the removal from the
registry and the walk up the chain. The tree is a root, its child and a grandchild made while one
cancel runs on the root and, in some configurations, another on the child. The SeqCst fences are
abstract (a TLA+ step is seen by every thread at once; the module docs and the loom models cover
the fences), and so is the registry's walk, taken as one read. 10 configurations: 3 that must
pass (one cancel; two at once; both, with every token dropped at the end), with a cancel that
returned leaving every descendant that existed as it began cancelled or freed
(`CancelReturnsWithSubtree`), a child made during a cancel cancelled once both returned, no walk
touching a freed node, a freed node in no registry, and with every token gone the whole tree freed;
5 mutations (a child reading its parent's flag before its insert stays uncancelled,
`RacingChildCancelled`; a walk that does not go into a child another cancel marked returns before
that subtree is cancelled, `CancelReturnsWithSubtree`; a walk using the weak handles without
upgrading them reaches a freed node, `NoTouchFreed`; a freed node left in its parent's registry,
`RegistryHoldsTheLive`; a freed node that does not let go of its parent leaks the chain, `NoLeak`);
and 2 witnesses (a new child cancelling itself; a walk going into a child another cancel had
marked). `lean/Nuclei/Cancel.lean` proves the free's walk up a chain of any length: letting go of
one handle frees exactly the nodes at the start of the chain that held their last handle, the next
one loses one handle and stays, no node with another handle is freed, and a chain of last handles
goes whole, whatever its length.

## The barging semaphore

`semaphore` (`tla/semaphore/Semaphore.tla`) checks the semaphore that `Semaphore`, `Mutex` (one
permit) and `ReentrantMutex` build on (`src/sync/raw_semaphore.rs`, with the acquire of
`src/sync/contend.rs` and the drain role of `src/sync/queue.rs`), at the grain of its atomic
steps: the state word (free permits, the closed bit, `QUEUED`, `NUDGE_OUT`, `HANDOFF`,
`WAKE_FRONT`, and the drain role's `DRAINING` and `DIRTY`), the owed count, the arrival stack and
the waiting list, and every waiter node (its phase, its nudge and owed marks, what a grant gave
it); each compare-and-swap of an attempt, an enqueue, a release, the owe-or-take and its
settling, a withdrawal and the close; and the drain's passes, each grant and each nudge. The
spin before queueing is abstract (more attempts of the kind other callers' barging already
makes), so is the node's waker handshake (the waiter machinery's part), and so is the list's
sweep of withdrawn waiters (one step). States that differ only in what nobody reads again are one
state: a pass's working values start over once read for the last time, and a node its future and
the queue are both done with is forgotten. 16 configurations: 7 that must
pass (a mutex locked three times past one wait; two tasks locking twice each, and one task locking
twice beside one locking once, where every task's ending is checked too; a wait given up
between two locks, with a handoff owed from the first loss; the same with two tasks, where every
task's ending is checked too; two permits with a request for both behind requests for one; a close
while two tasks lock), with the permits always adding up (`Conservation`) and all back at the end,
one holder at a time for a mutex, no waiter passed over more than `MAX_BYPASS` times while it
waits (`BypassBounded`), nobody barging while the handoff bit is up, nothing admitted after the
close, no waiter left on the arrival stack once the drain role is free (`NoStrandedArrival`), and
every task ending; the two tasks locking twice each and the wait given up between two locks check
each state once up to the names of the tasks that run the same program (a symmetry set,
`MC_SemaphoreSym.tla`); 6 mutations (a release that never asks for a pass leaves a waiter for good,
`AllTasksEnd`; a drain that gives up its role over a change made during its pass strands a
waiter that queued meanwhile, `NoStrandedArrival`; the last allowed loss raising no handoff lets
the waiter be passed over again, `BypassBounded`; an attempt that ignores the handoff bit barges
past the owed waiter, `NoBargePastOwed`; a waiter that leaves keeping what a grant gave it leaks
the permits, `PermitsBackAtEnd`; an attempt that ignores the closed bit is admitted after the
close, `NoAdmitAfterClose`); and 3 witnesses (the handoff raised; a grant given back by a waiter
that took on its own or left; a nudge finding its waiter still carrying the last one, which keeps
the marker up). Two of the model's first properties were stronger than the code's promises and
were corrected to them: a caller may take between a waiter's owe mark and its compare-and-swap,
which then raises the bit (`NoBargePastOwed` is about the bit), and a waiter granted while it
retried loses only to the permits taken for itself, which passes it over for nobody (the code
counts it, capped at `MAX_BYPASS`). `lean/Nuclei/Semaphore.lean` proves the pass's choice of whom
to nudge, for every line and budget: never more than the line, within the free permits (plus the
front's request after a release), an unfit front stops the walk, a release wakes the front, and
the walk stops at the first waiter that does not fit what those before it left, so a smaller
request behind a large front is never served first by a nudge.

## Notify

`notify` (`tla/notify/Notify.tla`) checks `Notify` (`src/sync/notify.rs`, on the drain role of
`src/sync/queue.rs`) at the grain of its atomic steps: the stored permit, `QUEUED` and the drain
role's bits; the generation and the owed counts of `notify_one` and `notify_last`; the arrival
stack and the waiting list; each node's phase and payload; and each transition, pass and grant,
with a waiter's first look (a moved generation, or the permit with nobody queued), its enqueue
and its drop. The node's waker handshake and the list's sweep are abstract, as in the semaphore
model. 12 configurations: 6 that must pass (two waiters and a `notify_waiters`, and a
`notify_one` then a `notify_waiters`, in any order, where every waiter made before the last
`notify_waiters` ends; two queued waiters and two `notify_one` calls, and a `notify_last` then a
`notify_one`, where both end; the same calls in any order, safety only, since two calls before
anybody waits store one permit; a waiter dropped beside one that awaits, which ends), with no
waiter done without a notification made since its `Notified` was (`NoSpuriousWake`), no
`notify_one` serving more than one waiter, and no waiter left on the arrival stack once the role
is free; 4 mutations (a `notify_waiters` that asks for no pass leaves its waiters,
`AwaitersMadeBeforeAWaitersCallEnd`; a pass that reads the generation before it takes the
arrivals wakes a newer arrival, `NoSpuriousWake`; a dropped waiter keeping a `notify_one`'s grant
leaves the other waiting, `EveryAwaiterEnds`; a drain giving up its role over a change made
during its pass strands an arrival, `NoStrandedArrival`); and 2 witnesses (a permit stored; a
grant passed on). `lean/Nuclei/Notify.lean` proves the pass's accounting of owed wakes for every
line and count (each served or left over for the permit, no more served than wait, something
left over only once every waiter was served) and that a masked generation is never one of the two
grant markers.

## The barrier

`barrier` (`tla/barrier/Barrier.tla`) checks the reusable barrier (`src/sync/barrier.rs`, on the
drain role of `src/sync/queue.rs`) at the grain of its atomic steps: the arrival count's
`fetch_add` (an arrival's generation, and whether it leads), the leader's transition, each
waiter's enqueue and drop, and each pass, which reads the current generation from the count after
it took the arrivals. The node's waker handshake and the list's sweep are abstract, as in the other
models of the drain role. 8 configurations: 3 that must pass (two tasks waiting twice at a barrier
of two, two generations; three tasks at a barrier of two, the third waiting for a second
generation by design; a wait dropped, its arrival still counted), with no wait completing before
its generation had every arrival (`NoEarlyRelease`), one leader per generation, no waiter left on
the arrival stack, and every wait of a finished generation that was not dropped completing; 4
mutations (a leader that asks for no pass leaves its generation waiting, `ReleasedTogether`; a
pass that reads the count before it takes the arrivals, and one that grants arrivals of the
current generation, release a wait early, `NoEarlyRelease`; a drain giving up its role over a
change strands an arrival, `NoStrandedArrival`); and 1 witness (a wait of the second generation
completes: the barrier is reused). `lean/Nuclei/Barrier.lean` proves the generation arithmetic for
every size: a generation is exactly `n` arrivals, its last leads it and only it, the leader's
arrival completes it, and a generation behind the count had every arrival.

## The event and the latch

`event` (`tla/event/Event.tla`) checks the one-shot `Event` and the `Latch` built on it
(`src/sync/event.rs`, `src/sync/latch.rs`): the `SET` bit and its transition (nothing when set,
a draining one when somebody is queued), each wait's first look, enqueue and drop, each pass,
and the latch's count with each compare-and-swap of `count_down`, the one that takes it to zero
setting the event. 7 configurations: 4 that must pass (two waiters and an event set twice; a
waiter dropped beside one that awaits; a latch of two counted down by two threads; and by three,
the third finding it open), with no waiter done before the set (`NoEarlyWake`), the latch never
open before its count reached zero, no waiter left on the arrival stack, every waiter not dropped
ending once the event is set, and the latch opening once counted; 3 mutations (a set with
waiters queued that asks for no pass leaves them, `AllWokenAfterSet`; a latch that sets the event
one count early, `LatchOpensAtZero`; a drain giving up its role over a change,
`NoStrandedArrival`).

## The once-cell

`oncecell` (`tla/oncecell/OnceCell.tla`) checks `OnceCell` (`src/sync/once_cell.rs`) at the grain
of its atomic steps: the value pointer (null, the slot's marker, or a racing builder's box) and
each compare-and-swap on it; the slot's write and its take-back after a lost publication; the turn
to write the slot (its permit, the close that ends every wait for it, and each wait); and each
asynchronous initializer's look, wait, second look, run (which may fail or be dropped) and
publication, each racing builder's, and each `set`. The turn's semaphore is abstract here (one
permit and a closed bit); its own protocol is the semaphore family's. `SetOnce` is a once-cell with
an `Event` beside it (the event family), and `Lazy` fills its cell as a racing builder does, so
both are covered by these two models. 10 configurations: 5 that must pass (two initializers; one
that fails, then one that succeeds; one dropped mid-run, then one that succeeds; an initializer, a
racing builder and a `set`; three initializers, one failing and one dropped, with a builder and a
`set`), with every call answering the one published value (`OneValue`), one writer of the slot at
a time, no initializer run over a value that was there as it took the turn, every call ending, and
the cell filled; 4 mutations (a publication that does not close the turn's semaphore leaves a
waiter for good, `EveryCallEnds`; an initializer that does not look again after taking the turn
runs over a builder's value, `NoInitAfterPublish`; a failed initializer that keeps the turn,
`EveryCallEnds`; a `set` that writes without the turn, `OneWriter`); and 1 witness (an initializer
that loses its publication to a builder and answers the builder's value).
`lean/Nuclei/OnceCell.lean` proves the pointer's state machine for every sequence of attempts: a
published value stays, every answer after it is that value, and from an empty cell the first
attempt wins.

## The reader-writer lock

`rwlock` (`tla/rwlock/RwLock.tla`) checks the lock that `RwLock` and `ReentrantRwLock` build on
(`src/sync/raw_rwlock.rs`, with the acquire of `src/sync/contend.rs` and the drain role of
`src/sync/queue.rs`) at the grain of its atomic steps: the state word (the readers, the writer,
upgradable and upgrading bits, and the queue's bits), the owed count, the arrival stack, the
waiting list and the upgrade's own slot, every node, each compare-and-swap of an attempt, an
enqueue, each kind's release, a downgrade, an upgrade (the upgrading bit, the write at once with
no reader inside, or the wait in its slot and the finish) and the owe-or-take, and each pass with
its upgrade grant, its handoffs in order (stopping after a writer) and its nudges (the front, and
the readers behind a sharing front). The spin, the node's waker handshake and the list's sweep
are abstract, as in the semaphore model, and so are the values nobody reads again (a pass's
working values start over once read for the last time, and a node its future and the queue are
both done with is forgotten). 15 configurations: 8 that must pass (reads beside a write;
writers; an upgrade beside a read; a downgrade beside a read; two upgradable reads; reads barging
past a waiting writer, which is owed a handoff in the end; a write dropped beside readers, with
three tasks for safety and with two for the ending too), with the state word matching exactly
who holds what (`StateMatches`), a writer alone and one upgradable read at most (`Exclusion`), no
read entering while an upgrade is under way, no upgrade finishing over a reader, the bypass bound,
no barging while a handoff is owed, no waiter left on the arrival stack, the lock free at the end,
no waiter still counted owed once every task ended (`OwedSettled`: the queue settles a node's
owed mark as it lets the node go, a withdrawn one included, as `release_queued` in
`src/sync/waiter/node.rs` does), and every task ending; the three-task drop checks each state once up to the two readers' names (a
symmetry set, `MC_RwLockSym.tla`); 5 mutations (a read that ignores the upgrading bit,
`NoReaderWhileUpgrading`; a pass that grants the upgrade with a reader inside,
`NoUpgradeOverReaders`; a release that never asks for a pass, `AllTasksEnd`; a drain giving up
its role over a change, `NoStrandedArrival`; a pass that counts the owed marks of the withdrawn
nodes it lets go after it swept the list, so an owed write withdrawn on the list stays counted,
`OwedSettled`); and 2 witnesses (an upgrade that waits in its slot;
a downgrade). `lean/Nuclei/RwLock.lean` proves the state word's rules for every state: each
admitted entry, each release, a downgrade and an upgrade's finish keep a writer alone with
upgrading tied to the upgradable read; an upgrade holds reads back; and a write needs the lock
empty.

## The reentrant locks

`reentrant` checks the two locks an owner may lock again, at the grain of their atomic steps.
`tla/reentrant/ReentrantMutex.tla` checks `ReentrantMutex` (`src/sync/reentrant_mutex.rs`, keyed
by `src/sync/owner.rs`): the holder id's loads and stores, the depth, and the permit's take, wait
and release, with the semaphore under it abstract (its protocol is the semaphore family's).
`tla/reentrant/ReentrantRwLock.tla` checks `ReentrantRwLock` (`src/sync/reentrant_rwlock.rs`): the
writer id, the write depth, each record's owner and reads, each lookup, a claim's look and its
compare-and-swap (which a racing claim can beat, and whose walk may miss a record freed behind
it), the push, and the plain lock's shares, with the plain lock abstract (its protocol is the
rwlock family's). 24 configurations: 5 that must pass (two owners that lock two deep, let one go
and lock again; one three deep beside one that locks twice; a writer that reads inside and lets
the write go first, a downgrade, beside a reader two deep; a writer that lets its read go first
beside a reader refused the write; two readers whose claims race for a freed record), with one
holder at a time and its depth its guards, a lock again only ever the holder's, a writer alone,
the plain lock's shares exactly the owners', every guard covered by a share, one record per
reading owner holding its read guards, the lock free at the end, and every owner ending; 13
mutations (the permit given back before the id is cleared, and every unlock clearing it,
`AllOwnersEnd`; the id never cleared, `Exclusion`, `FreeAtEnd` and `NoFalseReenter`; a lock again
that leaves the depth, `DepthMatches`; the last write given back though its owner reads,
`GuardsCovered`; the last read giving a share back under its owner's write, `SharesMatch`; a
reading owner's write not refused, `AllOwnersEnd`; a read taking no share beside any writer,
`Exclusion`; a claim without its compare-and-swap, `ReadsMatch`; a read that does not look for
its own record, `OneRecordEach`; the writer id kept, `FreeAtEnd`); and 6 witnesses (two deep; a
wait behind the holder; a refusal; a downgrade; a record claimed again; a claim beaten by a
racing one). `lean/Nuclei/Reentrant.lean` proves, for one owner at every depth (the TLA+ model
checks a few levels), that the mutex's owner holds the permit exactly while its depth is above
zero, and that the reader-writer lock's owner holds exactly the share its read and write depths
call for after any steps, a write being refused exactly when the owner reads without writing.

## The waiter node

`waiter` (`tla/waiter/Waiter.tla`) checks the node every contended future of the sync primitives
parks in their queues (`src/sync/waiter/`), at the grain of its state word's compare-and-swaps and
of each access to its two waker slots: a future arms the node and queues it, registers a new
waker in the slot a take under way is not reading and reads the marks in the swap that arms it,
spins with its waker disarmed, is polled without a wake (a combinator, say), marks itself owed,
and leaves by withdrawing; drain passes in turn nudge and grant it, claiming the armed waker for
the pass's wake list; each wake list takes the waker, claims again one armed meanwhile on a
granted or nudged node, and calls them beside the later passes; and the node goes back to the
pool with its last reference, for the next future to take. 16 configurations: 4 that must pass (a
spin and a spurious poll; a leave and an owed mark; two nudges; two futures through the pool),
with no slot touched by two parties at once, no party acting after it let go of its reference,
nothing referenced in the pool, the waker never armed while the future registers, the owed count
settling to zero, no waker claimed while the future spins, and everything ending with the
future's latest waker the one called; 7 mutations (a claim while the future registers and a
registration in the slot a take reads, `SlotExclusive`; a take that does not claim again and an
arming swap that does not read the marks, `AllEnd`; a spin that keeps its waker armed,
`NoClaimWhileSpinning`; a queue release that frees the node whoever holds it, `NoStaleAccess`; a
queue release that forgets the owed mark, `OwedSettled`); and 5 witnesses (a claim again, a mark
during a spin, a reuse through the pool, an owed mark settled by the queue, a leave that takes
back its armed waker). The registering test of `claimable` is implied by its armed test, since
every swap that sets the registering bit clears the armed one (`ArmedApart`): the mutation takes
both away. `lean/Nuclei/Waiter.lean` checks the word's own constants and expressions over all
4096 values: a claim takes the current slot, sets the taking bit and the wake reference and keeps
the phase, the marks and the other references, and a node just claimed is not claimable; the
register swap disarms, picks the slot a take under way is not reading and keeps the marks; the
arming swap arms and takes the nudge; a take that claims again keeps its bits, and one that ends
clears only its own.

## The run loop's looks at its driver

`loop` (`tla/loop/RunLoop.tla`) checks how often a shard's run loop looks at its IO driver
(`src/rt/shard.rs`, `Shard::block_on`). Each turn, in any order, either a task runs, or no
task runs and the future woke itself again, or nothing is ready and the loop spins and parks
(both look). Every `N` task runs bring a look without waiting, and so do every `N` turns of
the future's own. 4 configurations: 1 that must pass (the driver gets a look at least once
every `2N - 1` turns, `BoundedGap`), 1 mutation (not counting the future's own turns lets a
future that keeps waking itself starve the driver, breaking `BoundedGap`: the bug a test
caught, where IO such a future waited on never completed), and 2 witnesses (a look that came
from the future's turns; `2N - 2` turns without a look, so the bound is the tightest).

## How long queued IO waits on a busy shard

`cadence` (`tla/cadence/Cadence.tla`) checks how long an IO operation a task queued (a receive
pushed into the ring's submission queue) waits before the shard's run loop hands it to the
kernel (`src/rt/shard/run.rs`, the coarse clock in `src/rt/quartz.rs`). Each step one task runs,
quick, slow (a while, under the handoff threshold) or long (past it, so the shard is handed on
and the old runner is detached). The loop drives its IO every `N` runs, every `Q`th run when the
coarse clock says the last drive is older than the bound, and a runner that takes a shard over
drives before it runs a task. 6 configurations: 1 that must pass (whatever mix of tasks, the
operation waits at most `Bound`, `Bounded`), 2 mutations (no first drive: a shard handed on again
and again is looked at only every `Q`th run, so the wait is `Q` long tasks; no time rule: slow
tasks wait for the count of `N` runs; each breaks `Bounded`), and 3 witnesses (the first drive
submits, the time rule submits, the wait reaches the worst the guards allow, so `Bound` is not
slack). Time is whole units: a quick run adds none, a slow one `Slow`, a long one `Long`.

## What a ring lets into the kernel

`admission` (`tla/admission/Admission.tla`) checks how a shard's ring (`src/rt/shard/ring.rs`,
`src/rt/shard/ring/exits.rs`) admits operations: a completion queue of `C` slots, a receive group
whose buffers are arrivals' share of it, a cap on the operations counted in the kernel, long-lived
operations (every operation of a source, each waiting on its peer: multishot receives and accepts,
which never end by themselves; one-shot receives into a buffer, polls for bytes and accepts, which
end when their peer sends or connects; sends, polls for room and connects under way, which end when
their peer reads or answers, and a zero-copy send that holds its buffer until the kernel's
notification, which comes once the peer acknowledged the bytes; and a child's exit watch) and
finite ones (a file's reads and writes) that wait in room when the cap is used. Before, the ring
counted the one-shot operations of a side that reads: in the vs-tokio benchmark 3840 idle
connections, the cap of a ring of 2048 entries beside its 256-buffer group, filled it with one-shot
receives that waited for their peers, and the next accept waited in room behind them for good,
leaving every connection past them unaccepted. It counted every operation of a side that writes
too, so sends to peers that never read could fill the cap the same way and hold every other
connection's reply, and every file call, in room for good. 9 configurations: 1 that must pass (the
group cut to half the queue, long-lived operations of every kind not counted: a finite operation
never waits in room while none is in the kernel, `NoRoomDeadlock`, and arrivals plus finite
completions fit the queue, `CqNeverOverfull`), 4 mutations (the group not cut breaks
`CqNeverOverfull`; multishot operations counted, one-shot ones of a side that reads counted, and a
side that writes counted, as they were, let idle or deaf peers use the cap and break
`NoRoomDeadlock`), and 4 witnesses (a finite operation does wait in room; a multishot one, a
one-shot one, and a write side's is let in past the cap). An accept's connections and the end of
each long-lived operation are not modelled: no room is kept for them, and past the queue the kernel
holds them (`IORING_FEAT_NODROP`) until the shard takes them. `tests/idle_peers.rs` fills the cap
of the smallest ring with receives of idle peers, and then with sends to peers that never read, and
checks every other peer is still accepted and answered.

## A vectored read's fill

`lean/Nuclei/Fill.lean` checks how a vectored read's count is shared out among its buffers
(`src/buf/vectored.rs`, `Vectored::set_len`): the kernel writes one read's bytes into the
buffers' rooms in order, and each room, in order, takes what is left of the count up to its
size. For every list of rooms and every count it proves that each room takes at most its size
(`fill_fits`), that the rooms take the whole count or all their room (`fill_total`), and that a
room not filled is the last that takes anything (`fill_in_order`), with `fill_length` and
`fill_zero` beside them. Its mutation, every room taking the whole count, makes up bytes. The
bytes themselves, and the kernel's rule that a vectored read fills its descriptors in order, are
left out. The property test `buf::tests::vectored::a_read_fills_rooms_in_order` checks the Rust
code against the same rules.

## A ring's overflow count

`lean/Nuclei/Overflow.lean` checks how a ring's poller counts completion-queue overflows for
`stats` (`src/rt/park/ring.rs`): it looks at the kernel's overflow flag before each wait's
enter and after it took the completions, and counts a look that finds the flag set after one
that found it clear. Over every sequence of looks it proves that an overflow counts once however
many waits it lasts (`stutter`), that the count never passes the looks that saw the flag set
(`at_most_seen`), that a set look after a clear one counts (`counted_after_clear`), and that two
stretches of looks add up when the first ends clear (`split_at_clear`). Its witness shows why
the look after the completions is there: with it two floods count two, and without it they count
one; its mutation, counting every set look, breaks `stutter`. An overflow that begins and ends
between two looks is seen by neither the code nor the model. `tests/cq_overflow.rs` overflows
the smallest ring's queue for real: while its shard's thread is held, twice the queue's entries
of peers connect to four listeners, each with a multishot accept armed, and send a request each,
and once the thread enters the kernel again the accepts fill the queue and the last completions
go past it. The test checks the overflow was counted and that every peer is accepted and gets
its own bytes back.

## A shard handed to another thread

`handoff` (`tla/handoff/Handoff.tla`) checks the shard handoff (`src/rt/handoff/`,
`src/rt/context.rs`) on a machine whose stores wait in a store buffer of two entries, as x86's do:
a runner leaves its shard for a task's poll (a release store of its word), comes back at the
poll's end and at each call into the runtime from the task (a store of its word, the compiler's
fence, a load of its claim), and cancels a pending claim with a compare-and-swap; the monitor
sets the claim pending, runs the heavy fence (every buffer flushed at once), reads the word again
and takes with the opposite compare-and-swap, then hands the shard to a spare with a fresh slot.
16 configurations: 2 that must pass (two handoffs, a remote push and a bound task: at most one
thread inside the shard, `Exclusive`, a detached thread staying out, a bound task on its creator
only; a first poll that never returns, every remote push run anyway, `Rescued`); 6 mutations,
each of which must break a property (no heavy fence, no second read of the word, no look at the
claim at a poll's end or in a call into the runtime, the old runner's slot handed on: each breaks
`Exclusive`; the new runner taking the bound tasks breaks `BoundOnCreator`; no handoff at all
breaks `Rescued`); and 4 witnesses (a handoff, a cancel by the monitor, a call into the runtime
from a detached thread, a cancel by the runner). `tla/handoff/Retarget.tla` checks the waker of a
`block_on` future moved to its thread's own shard: the waker sets its flag and reads its target,
the mover sets the target and looks at the flag, both sequentially consistent (1 that must pass,
1 mutation with relaxed accesses that loses the wake, 1 witness of a wake that went to the old
shard). `lean/Nuclei/Handoff.lean` proves the word's packing (its epoch and its side come back
out, a new poll writes a word no earlier poll wrote, every epoch below `2^63` fits) and the watch:
the monitor's streak of four is five equal reads outside the shard, which a tick apart or more
mean the poll has run at least four ticks, and a read that missed the poll just before one that
saw it means the poll began after it. The model takes a bound task's home as its creator thread
and a hand-over to a spare as one step.

## A file's blocking lock

`flock` (`tla/flock/FileLock.tla`) checks the cancellation of `Handle<File>::lock_file`
(`src/fs/lock.rs`: `lock_file` and its `LockCall`) at the grain of its atomic steps. `lock_file`
waits in the kernel until the file is free, so it is a job of the blocking pool; a future dropped while the job
waits must not leave the file locked with nobody to give it back. The future and the job share one
word, WAITING, HELD or GONE. The job locks the file and then does one compare-and-swap from
WAITING to HELD, and unlocks the file itself if it finds GONE; a future dropped before it took the
result swaps GONE in, and unlocks the file itself if the word was HELD; a job that has not started
when the future is dropped is cancelled. The model has each step: the word and its swap and
compare-and-swap, the job's claim, the kernel's lock and each unlock, the drop at any point and
its three steps, and the future taking the result. The kernel's lock is one flag: other holders
of the file, and how long the blocking lock waits, are the kernel's. 7 configurations: 2 that
must pass (a lock that always succeeds, and one that may fail, each with the future dropped at any
point), with the file locked, once both are done, only if the caller holds the lock
(`NoOwnerlessLock`), a lock handed to the caller held, and both parties always ending; 3
mutations (a job that writes HELD without looking for GONE, a drop that finds HELD and does not
unlock, a drop that reads the word and writes GONE in two steps, each `NoOwnerlessLock`); and 2
witnesses (the job gets the lock after its future was dropped, and a dropped future finds the
lock held and gives it back itself).

## Many reads in one call

`many` (`tla/many/ReadMany.tla`) checks `Handle<File>::read_many_at` (`src/fs/file/many.rs`:
`FileIo::read_many_at`, with `poll_ordered` of `src/fs/file/positional.rs`): a call of N reads at
offsets of the task's own, with a cursor write accepted before it and not landed yet. The call
waits until that write landed, then sends reads out in the order the task named them, at most
`Cap` at once (`MANY_AT_ONCE`); the kernel makes them in any order, and each time one ends the call
takes its result into that read's place and sends the next. A read moves one cell; the ring's own
room only delays a read and is the kernel's. 8 configurations: 3 that must pass (four reads
through a window of two, three through a window of one, three through a window wider than the
call), with `WithinCap`, never more reads out than the window holds; `ResultInPlace`, each place
holds its own read's result, which saw the write before the call; `EachOnce`, no read goes out
twice; `AllAnswered`, a call that is done answered every read; and `Termination`; 4 mutations (a
call that sends every read out whatever the window holds breaks `WithinCap`; one that gives
results back in the order reads end, and one that sends reads before the write before it landed,
break `ResultInPlace`; one that does not move on to the next read breaks `EachOnce`); and 1 witness
(a read ends before one named before it).

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
- **The newer families' abstractions.** The signal model leaves out the handler's
  async-signal-safety and its errno (code review and the signal tests' part); the process
  models abstract the kernel's reap to one step; the port model abstracts Windows'
  refusal to rebind a handle with operations out to a bind that fails while any are; the
  broadcast model leaves the wake to `Notify` (checked with the sync primitives) and the
  reclamation of nodes to kovan.
- **How soon.** `Termination` says every waiter is eventually served under weak fairness; it
  says nothing about how long that takes, and nothing about a thread the scheduler stops for
  good.

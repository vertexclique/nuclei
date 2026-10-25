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
| TLA+, checked by TLC (`tla/`) | the bounded channel at the grain of its atomic steps: the semaphore's state word, the arrival stack, the waiting line and its set-aside, every waiter node, the drain pass, the waker handshake, cancellation, permits, the receiver's receive, close and drop; the one-shot channel, the runtime's task, the park handshake, the remote timer, the guard of an IO source's side, the reads and writes from a task's own buffer and a source's operations on its shard's ring the same way (sections below) | every order of every thread's steps, for the slots, senders and programs each configuration sets |
| Lean 4 (`lean/`) | the bookkeeping of the fair semaphore: conservation of slots, first come first served, the hand-on of a dropped waiter's share, the close; the one-shot channel's ownership, the task's references, future and output, the timer wheel's arithmetic and a ring's file slots (sections below) | every sequence of operations and every size, for a model of one thread doing one whole operation at a time |

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
runnable is left to keep itself and the shard for good: 2 that must pass, 3 mutations that
break `NothingLeft` (a pusher looking before it pushes, one that leaves its runnable, a single
drain pass), and 2 witnesses. TLA+ steps are
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
it, it tries the write side the same way, and with both it performs the close and drops its
object. Taking fewer, it lets go and hands its object to the source, never waiting. Sides are
taken in one order: a holder of the read side that sees the request bit tries the write side; a
holder of the write side lets go first, then tries the read side once. Whoever performs the
close marks it done, and whichever of the dropper's hand-over and that mark comes second drops
the object. The close lingers: once the dropper is done, a task drains what the handle wrote
through a copy of the descriptor, holding the write side; finding the close not performed yet,
it waits on the write side, and the performer wakes the write side's waiters rather than
forget them as it does the handle's others. 11 configurations: 2 that must pass
(`NoUseOfClosed`, no holder uses a descriptor closed under it; `PerformedOnce`; `Closed`, once
everything else is done the close was performed and the object dropped; and liveness), 6
mutations (a holder that ignores the request bit, or takes a raised bit for its own, breaks
`Closed`; a dropper that performs holding one side, or drops its object before the close is
done, breaks `NoUseOfClosed`; a holder of the write side that tries the read side while still
holding its own livelocks, and a close that forgets the lingering drain's wake strands it, both
breaking `Termination`), and 3 witnesses (a holder performs the close; the dropper hands its
object over; the lingering drain waits before the close is performed). The Rust tests in
`src/io/source/tests/requests.rs` hold a side from the test and check the same on real sockets,
each with a mutation of the code that makes one of them fail.

## Reads and writes from a task's own buffer

`lent` (`tla/lent/LentRead.tla`, `tla/lent/LentWrite.tla`, `tla/lent/LentFile.tla`) checks
`read_owned` and `write_owned` on streams (`src/io/source/read.rs`, `src/io/source/write.rs`),
where the task hands the side its own buffer and the driver reads into it or writes from it
with no copy, and on files (`src/fs/file.rs`), where the buffer goes to a job of the blocking
pool. The guards are abstract here (a step under one runs whole; `side` checks the side's).

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
withdrawn write).

The file (`LentFile.tla`): the file has a cursor and an epoch, which every write and seek
moves on, and its jobs run on the blocking pool in any order. Two tasks each read one cell at
the cursor into their own buffers: a read waits for the write in flight, its job reads at the
cursor, and its result counts only if the cursor and the epoch are still those it read at, else
it reads again. A third task writes a cell from the pool, then one from its own buffer (which
waits for the write in flight and then is that write), then seeks back to the start. Any task
may go away while its job is out: a read job is cancelled or its result dropped, and a lent
write still lands. 9 configurations: 3 that must pass (tasks that may go away and tasks that
stay, with `NoStaleRead`, a read counts only a value its cell still holds; `AllLanded`, every
accepted write is in the file at the end; and liveness), 3 mutations (a result that counts on
the cursor alone breaks `NoStaleRead`, through the seek back; a lent write that goes in flight
over the one there and a lent write cancelled when its task goes away break `AllLanded`), and 3
witnesses (a read that reads again, the write of a task that went away landing, a read after
the seek that counts the lent write's value). In Rust a seek needs the handle exclusively, so
it cannot overlap a read; the model lets it, so the epoch rule is checked for any path that
moves the cursor back.

The Rust tests in `src/io/source/tests/owned.rs` and `src/fs/tests/owned.rs` check the same
rules on real sockets and files, each with a mutation of the code that makes one of them fail.

## A source's operations on its shard's ring

`ring` (`tla/ring/RingOp.tla`) checks how a side of a source works on an io_uring ring
(`src/io/source/ring.rs`, `src/io/source/write/ring.rs`, `src/rt/shard/ring.rs`) when the
side is held from another thread than the ring's. A task there writes twice; each write is
accepted into the side, and the side's operation goes to the ring one at a time. Only the
ring's own thread submits: a task elsewhere posts a kick once it has let go of the side, and
the ring's thread submits what the side wants. Submitting raises the side's flag (an operation
is out) and the source's count of operations in the kernel. When the kernel completes one, the
ring's thread lowers the count, leaves the completion in the side's one-slot mailbox and
defers it to the side: applied at once if the side is free, else by its holder as it lets go,
which then asks for the next operation. A retire deregisters the source, cancels what is out
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

`lean/Nuclei/Files.lean` checks the ring's file slots (`src/syscore/linux/iouring/files.rs`):
every entry names its source's file by a slot of the kernel's fixed-file table, or past a full
table by a slot holding the ring's own copy of the descriptor, and counts on that slot from its
submission to its completion. For every sequence of operations from a fresh table it proves
that a free slot is held by nobody, named by no entry and holds no open copy (`inv_run`); that
a slot a source takes has no entry naming it, so an entry on its way never reaches the next
source's file (`taken_slot_is_unnamed`); that a free slot is never freed again
(`no_double_free`); and that a copy closes only once no source holds it and no entry names it
(`copy_closes_unnamed`); and it computes the worked example of a slot released with a read out
and handed out again only once the read completed (`example_reuse_waits`). The Rust tests in
`src/rt/shard/ring/tests.rs` run sources past a full table, a retire with a read out and a
shard that goes with one, each with a mutation of the code that makes one of them fail.

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
ring a copy of the descriptor; the ring's thread puts the file into its table and hands the
slot back as a request, which whoever performs it (both sides held) keeps if the attach is
still wanted, or lets go. 9 configurations: 4 that must pass (a close, an evict, both, and
liveness), 2 mutations that leak the file into the table of a closed source (`NoLeak`), and 3
witnesses.

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
closes the channel. The receiver looks at its next position's slot: its own position is a value
taken, a later one is a lag (it goes on from the tail, read then, less the ring's size, counting
what it skipped), anything else is nothing yet, unless the channel is closed and its next
position is the tail. 7 configurations: 2 that must pass (what was taken plus what was
reported lost is every value sent; the receiver reaches the close), 3 mutations (a send
publishing over a later position leaves the receiver waiting for good, `Finishes`; stopping at
the close with values in the ring and an uncounted lag break `Counted`), and 2 witnesses (a
lag; a send that finds a later position in its slot). `lean/Nuclei/Broadcast.lean` proves the
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
`Notified` is abstracted to what `Notify` promises: it hears every wake made after it was made. 12
configurations: 4 that must pass (sends; modifying sends, where no change is lost; the two mixed;
a sender waiting for the receivers to go, which ends), with `changed` never answering closed
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
between two looks is seen by neither the code nor the model.

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

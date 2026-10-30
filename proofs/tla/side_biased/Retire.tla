------------------------------- MODULE Retire -------------------------------
(***************************************************************************)
(* The operation count kept plain on its ring (Phase 2 of the network-path *)
(* plan), on a machine whose stores wait in a store buffer, as x86's do.   *)
(* No code is written yet. Today `Source::op_out` and `Source::op_done`    *)
(* in src/io/source/ops.rs change `ops_out` with SeqCst read-modify-writes *)
(* (locked instructions), and `op_done` reads `awaits_ops` after its       *)
(* decrement; `Source::retire` in src/io/source/requests.rs registers its  *)
(* waker, stores `awaits_ops` SeqCst and reads the count (`ops_idle`). The *)
(* plan keeps the same order and changes what it costs: a ring's increment *)
(* and decrement become plain stores, and a thread that waits for the      *)
(* count runs the heavy fence (`heavy` in src/rt/handoff/fence.rs) between *)
(* its flag and its look at the count. A plain count needs one writer at a *)
(* time, so a source's re-home (`evict_now` and the re-attach, in          *)
(* src/io/source/requests.rs) waits, on the new ring's thread, until the   *)
(* count is zero before the new ring may submit (`ops_registered`, then    *)
(* `submit_wanted` on each side): a side may still have an operation out   *)
(* on the old ring, whose `op_done` runs on the old ring's thread. The     *)
(* Windows port keeps today's atomic count and is not modelled here.       *)
(*                                                                          *)
(* Who acts. Two rings' threads: ring A, the source's first home, and ring *)
(* B, where it moves. Each submits the source's operations to the kernel  *)
(* while the source is its own and processes their completions. The kernel *)
(* completes what it holds. A retire runs on another thread: it must not  *)
(* let the IO object go while the kernel may still use it. A flusher       *)
(* stands for the memory: at any moment the oldest store of any buffer     *)
(* lands.                                                                  *)
(*                                                                          *)
(* What can happen. A submit stores count + 1 (plain, into its ring's      *)
(* buffer) and hands the operation to the kernel at once. A completion's   *)
(* `op_done`, on the ring the operation went to, stores count - 1 (plain), *)
(* then, with a compiler fence only, reads the waiters' flags with plain   *)
(* loads: the count it stored is zero and a flag is up, it takes that      *)
(* waiter's waker (a locked instruction) and wakes it. The re-home moves   *)
(* the source to ring B (ring A submits no more), then waits as the        *)
(* retire does: ring B registers its waker, stores its flag (plain), runs  *)
(* the heavy fence and reads the count; zero, the count is ring B's and it *)
(* may submit; else it waits until woken and looks again. The retire has   *)
(* its request performed (no operation is submitted after it) and waits    *)
(* the same way for zero; then it lets the object go.                      *)
(*                                                                          *)
(* What must never happen. The count differs from the operations really   *)
(* out (a lost update). The retire lets the object go while the kernel     *)
(* holds one of the source's operations, or before its ring processed a   *)
(* completion. A thread that waits for zero is never woken.                *)
(*                                                                          *)
(* What is abstract. The retire's request (`Source::request` with RETIRE)  *)
(* and the re-home's move are one step each: each is performed by a holder *)
(* of both sides (Requests.tla checks them), and a submit happens under a  *)
(* side guard, so no submit of the old home comes after either; this model *)
(* assumes nothing of how they order memory. A waker is one slot,          *)
(* registered and taken with locked instructions (`AtomicWaker`). One      *)
(* retire, one re-home.                                                    *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC                                             \* numbers, sequences (the buffers), TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    OpsA,                                                                    \* the operations ring A may submit
    OpsB,                                                                    \* the operations ring B may submit once the source is its
    Rehome,                                                                  \* whether the source moves from ring A to ring B
    Mutation                                                                 \* "none", or the name of the one rule this run takes away

Flusher == 0                                                                 \* the memory, draining buffers
RingA == 1                                                                   \* ring A's thread: the first home
RingB == 2                                                                   \* ring B's thread: where the source moves
Retirer == 3                                                                 \* the thread that retires the source
Kernel == 5                                                                  \* the kernel
Rings == {RingA, RingB}                                                      \* the two rings' threads
Writers == {RingA, RingB, Retirer}                                           \* the threads with a store buffer
BufCap == 2                                                                  \* a store buffer holds this many stores; a store into a full one waits for its oldest
OpsOf(r) == IF r = RingA THEN OpsA ELSE OpsB                                 \* the operations ring `r` may submit

\* The value thread buffer `sq` gives a load of word `w`: its own newest store of `w`, else memory's.
ReadAt(m, sq, w) ==                                                          \* the load's value:
    LET idx == {i \in DOMAIN sq : sq[i][1] = w}                              \* the buffered stores of `w`
    IN IF idx = {} THEN m[w]                                                 \* none: memory answers
       ELSE sq[CHOOSE i \in idx : \A j \in idx : j <= i][2]                  \* else the newest one

\* Memory `m` after the stores of buffer `sq` land, oldest first.
RECURSIVE Apply(_, _)                                                        \* Apply calls itself, one store at a time
Apply(m, sq) ==                                                              \* memory once the buffer landed:
    IF sq = <<>> THEN m                                                      \* nothing left to land
    ELSE Apply([m EXCEPT ![Head(sq)[1]] = Head(sq)[2]], Tail(sq))            \* the oldest lands, then the rest

(* --algorithm Retire                                                          \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    mem = [count |-> 0, flag |-> 0, hflag |-> 0],   \* memory's plain words: the count, the retire's flag, the re-home's flag
    buf = [t \in Writers |-> <<>>],         \* each thread's store buffer, oldest first, of <<word, value>> pairs
    out = 0,                                \* (ghost) the operations really out: submitted, and not yet counted down
    submitted = [r \in Rings |-> 0],        \* the operations each ring submitted
    inKernel = [r \in Rings |-> 0],         \* the operations the kernel holds now, by ring
    posted = [r \in Rings |-> 0],           \* completions posted to each ring and not processed yet
    home = RingA,                           \* the ring the source is registered with
    owns = FALSE,                           \* ring B saw the count at zero after the move: the count is its
    detached = FALSE,                       \* the retire's request was performed: no operation is submitted after it
    waker = FALSE,                          \* the retire's waker is registered
    wokenR = FALSE,                         \* the retire was woken and has not looked again yet
    hwaker = FALSE,                         \* the re-home's waker is registered
    wokenH = FALSE,                         \* the re-home was woken and has not looked again yet
    freed = FALSE,                          \* the retire let the IO object go
    ownerWoke = FALSE,                      \* (record) an op_done woke a sleeping retire
    rehomeWaited = FALSE;                   \* (record) an op_done on ring A woke a re-home waiting for zero

define                                                                       \* names the code and the properties use:
    \* Ring `r` may submit: the source is its, its wait (for ring B) is over, no retire came, and it has some left.
    MaySubmit(r) ==                                                          \* ring `r` may submit when:
        /\ home = r                                                          \* the source is its,
        \* MUTATION rehome_early_submit: ring B submits as soon as the source is its, before the count is zero.
        /\ (r = RingA \/ owns \/ Mutation = "rehome_early_submit")           \* the count is its (ring A first),
        /\ ~detached                                                         \* no retire came,
        /\ submitted[r] < OpsOf(r)                                           \* and it has operations left
    \* Ring `r` can submit nothing more and has processed every completion.
    RingFinished(r) ==                                                       \* ring `r` is finished when:
        /\ (submitted[r] = OpsOf(r) \/ detached                              \* it can submit nothing more:
            \/ (r = RingA /\ home = RingB)                                   \* (ring A after the move,
            \/ (r = RingB /\ ~Rehome))                                       \* ring B with no move),
        /\ inKernel[r] = 0 /\ posted[r] = 0                                  \* and nothing of its is out
    \* Every process but the flusher finished.
    Quiet == \A p \in {RingA, RingB, Retirer, Kernel} : pc[p] = "Done"       \* no step left
    \* Every buffer is empty.
    Drained == \A t \in Writers : buf[t] = <<>>                              \* no store waiting
end define;                                                                  \* end of the names

\* A plain store by thread `t` of `v` into word `w`: it joins the thread's buffer.
macro Store(t, w, v) begin                                                   \* its steps:
    if Len(buf[t]) = BufCap then                                             \* a full buffer: the CPU waits for its oldest store,
        mem := [mem EXCEPT ![Head(buf[t])[1]] = Head(buf[t])[2]];            \* which lands in memory,
        buf[t] := Append(Tail(buf[t]), <<w, v>>);                            \* and the new store joins the end
    else                                                                     \* room left:
        buf[t] := Append(buf[t], <<w, v>>);                                  \* the new store joins the end
    end if;                                                                  \* either way the store waits in the buffer
end macro;                                                                   \* end of the plain store

\* A locked instruction of thread `t`: its buffer drains before it acts.
macro Lock(t) begin                                                          \* its steps:
    mem := Apply(mem, buf[t]);                                               \* every store it buffered lands, oldest first
    buf[t] := <<>>;                                                          \* and its buffer is empty
end macro;                                                                   \* end of the locked instruction

\* The heavy fence: every buffered store of every thread lands.
macro HeavyFence() begin                                                     \* its steps:
    mem := Apply(Apply(Apply(mem, buf[RingA]), buf[RingB]), buf[Retirer]);   \* every buffer lands,
    buf := [t \in Writers |-> <<>>];                                         \* and every buffer is empty
end macro;                                                                   \* end of the heavy fence

\* The memory: the oldest store of any buffer lands, at any time, until everything is finished.
fair process flusher = Flusher                                               \* weakly fair: a buffered store lands in the end
begin                                                                        \* the memory's steps:
Flush:                                                                       \* one store lands
    while ~(Quiet /\ Drained) do                                             \* while anything runs or any store waits
        with t \in {u \in Writers : buf[u] # <<>>} do                        \* some thread with a buffered store:
            mem := [mem EXCEPT ![Head(buf[t])[1]] = Head(buf[t])[2]];        \* its oldest store lands
            buf[t] := Tail(buf[t]);                                          \* and leaves the buffer
        end with;                                                            \* one store per step
    end while;                                                               \* everything finished and landed
end process;                                                                 \* end of the memory

\* The kernel: it completes, in its own time, what it holds, and posts each completion to its ring.
fair process kernel = Kernel                                                 \* weakly fair: what it holds completes in the end
begin                                                                        \* the kernel's steps:
K_loop:                                                                      \* one completion
    while TRUE do                                                            \* for as long as work may come:
        await (\E r \in Rings : inKernel[r] > 0)                             \* it holds an operation,
              \/ (pc[RingA] = "Done" /\ pc[RingB] = "Done");                 \* or none can come
        if \A r \in Rings : inKernel[r] = 0 then goto Done;                  \* the rings finished and nothing is held
        else                                                                 \* it holds an operation:
            with r \in {q \in Rings : inKernel[q] > 0} do                    \* of some ring:
                inKernel[r] := inKernel[r] - 1;                              \* done with the object,
                posted[r] := posted[r] + 1;                                  \* and posted to that ring
            end with;                                                        \* one completion per step
        end if;                                                              \* the next
    end while;                                                               \* nothing more will come
end process;                                                                 \* end of the kernel

\* A ring's thread: ring B first moves the source and waits for the count; then each submits (`op_out`) and
\* processes completions (`op_done`).
fair process ring \in Rings                                                  \* one process per ring, weakly fair
variables zero = FALSE, sawFlag = 0, sawHflag = 0;                           \* the count it stored was zero; the flags it read
begin                                                                        \* a ring's steps:
G_start:                                                                     \* where each ring begins
    if self = RingA \/ ~Rehome then goto G_loop; end if;                     \* ring A works at once; ring B with no move too
H_move:                                                                      \* the re-home's move (`evict_now`, then the re-attach)
    if detached then goto G_loop;                                            \* a retire came first: no move
    else home := RingB;                                                      \* the source is ring B's: ring A submits no more
    end if;                                                                  \* now the wait for zero
H_reg:                                                                       \* the re-home's waker
    if Mutation = "rehome_early_submit" then goto G_loop; end if;            \* (mutated) no wait at all
H_lock:                                                                      \* the waker's registration
    Lock(self);                                                              \* the waker registers with a compare-and-swap
    hwaker := TRUE;                                                          \* and is in its slot
H_flag:                                                                      \* the re-home's flag
    Store(self, "hflag", 1);                                                 \* hflag := 1, a plain store into ring B's buffer
H_fence:                                                                     \* the re-home's heavy fence
    \* MUTATION rehome_no_fence: ring B reads the count with no heavy fence after its flag.
    if Mutation # "rehome_no_fence" then HeavyFence(); end if;               \* every buffer drains (unless mutated)
H_count:                                                                     \* the re-home's look at the count
    if ReadAt(mem, buf[self], "count") = 0 then                              \* zero: nothing out on ring A
        owns := TRUE;                                                        \* the count is ring B's,
        goto H_lower;                                                        \* and the flag goes down
    end if;                                                                  \* else it waits
H_sleep:                                                                     \* waiting for zero
    await wokenH;                                                            \* ring A's last op_done woke it
    wokenH := FALSE;                                                         \* the wake is taken
    goto H_lock;                                                             \* it looks again, the same way
H_lower:                                                                     \* the re-home is over
    Store(self, "hflag", 0);                                                 \* its flag goes down, a plain store
G_loop:                                                                      \* the ring's next piece of work
    either                                                                   \* either a submit:
        await MaySubmit(self);                                               \* under a side guard, while the source is its
        Store(self, "count", ReadAt(mem, buf[self], "count") + 1);           \* count + 1, a plain store into its buffer,
        submitted[self] := submitted[self] + 1;                              \* the operation submitted,
        inKernel[self] := inKernel[self] + 1;                                \* the kernel holds it at once,
        out := out + 1;                                                      \* (ghost) one more really out
        goto G_loop;                                                         \* then the next piece of work
    or                                                                       \* or a completion:
        await posted[self] > 0;                                              \* one is posted to this ring:
        posted[self] := posted[self] - 1;                                    \* it takes it (`op_done`)
    or                                                                       \* or nothing is left:
        await RingFinished(self);                                            \* no submit can come, no completion waits
        goto Done;                                                           \* the ring is finished with the source
    end either;                                                              \* a completion is being processed
OD_first:                                                                    \* (mutated) the flags read first
    \* MUTATION flag_before_dec: op_done reads the waiters' flags before its decrement, not after.
    if Mutation = "flag_before_dec" then                                     \* (mutated) plain loads of the flags
        sawFlag := ReadAt(mem, buf[self], "flag");                           \* the retire's,
        sawHflag := ReadAt(mem, buf[self], "hflag");                         \* and the re-home's, ahead of the decrement
    end if;                                                                  \* else nothing here
OD_dec:                                                                      \* op_done's decrement
    zero := ReadAt(mem, buf[self], "count") = 1;                             \* the count it is about to store is zero (it reads 1)
    Store(self, "count", ReadAt(mem, buf[self], "count") - 1);               \* count - 1, a plain store into its buffer
    out := out - 1;                                                          \* (ghost) one fewer really out
OD_look:                                                                     \* op_done's look at the flags
    \* A compiler fence only, then plain loads of the flags.
    if Mutation # "flag_before_dec" then                                     \* in order: the flags after the decrement
        sawFlag := ReadAt(mem, buf[self], "flag");                           \* the retire's,
        sawHflag := ReadAt(mem, buf[self], "hflag");                         \* and the re-home's
    end if;                                                                  \* (mutated: read before)
    if ~zero \/ (sawFlag = 0 /\ sawHflag = 0) then goto G_loop; end if;      \* operations still out, or nobody waits
OD_wake:                                                                     \* op_done takes the wakers of those that wait
    Lock(self);                                                              \* the wakers are taken with locked instructions
    if sawFlag = 1 /\ waker then                                             \* the retire waits:
        waker := FALSE;                                                      \* its waker taken,
        wokenR := TRUE;                                                      \* and it is woken
        if pc[Retirer] = "R_sleep" then ownerWoke := TRUE; end if;           \* (record) a sleeping retire
    end if;                                                                  \* the retire is done with
    if sawHflag = 1 /\ hwaker then                                           \* the re-home waits:
        hwaker := FALSE;                                                     \* its waker taken,
        wokenH := TRUE;                                                      \* and it is woken
        if pc[RingB] = "H_sleep" then rehomeWaited := TRUE; end if;          \* (record) a sleeping re-home
    end if;                                                                  \* the re-home is done with
    goto G_loop;                                                             \* the completion is processed
end process;                                                                 \* end of a ring

\* The retire, on another thread (`Source::retire`).
fair process retirer = Retirer                                               \* weakly fair, like every process
variables early = 0;                                                         \* the count read before the flag (mutated)
begin                                                                        \* the retire's steps:
R_req:                                                                       \* the retire's request
    detached := TRUE;                                                        \* performed with both sides held: no submit comes after
R_early:                                                                     \* (mutated) the count read first
    \* MUTATION count_before_flag: the retire reads the count before it raises its flag.
    if Mutation = "count_before_flag" then                                   \* (mutated) a plain load of the count
        early := ReadAt(mem, buf[Retirer], "count");                         \* ahead of the flag and the fence
    end if;                                                                  \* else nothing here
R_reg:                                                                       \* the retire's waker
    Lock(Retirer);                                                           \* the waker registers with a compare-and-swap
    waker := TRUE;                                                           \* and is in its slot
R_flag:                                                                      \* the retire's flag
    Store(Retirer, "flag", 1);                                               \* flag := 1, a plain store into its buffer
R_fence:                                                                     \* the retire's heavy fence
    \* MUTATION no_fence: the retire reads the count with no heavy fence after its flag.
    if Mutation # "no_fence" then HeavyFence(); end if;                      \* every buffer drains (unless mutated)
R_count:                                                                     \* the retire's look at the count
    if (IF Mutation = "count_before_flag" THEN early                         \* none out by the count it goes by:
        ELSE ReadAt(mem, buf[Retirer], "count")) = 0 then goto R_free; end if; \* the object may go
R_sleep:                                                                     \* asleep until woken
    await wokenR;                                                            \* an op_done woke it
    wokenR := FALSE;                                                         \* the wake is taken
    goto R_early;                                                            \* it looks again, the same way
R_free:                                                                      \* the object goes
    freed := TRUE;                                                           \* the retire lets the IO object go
    Store(Retirer, "flag", 0);                                               \* and lowers its flag, a plain store, as today
end process;                                                                 \* end of the retire
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* No lost update: once every store has landed, the count is the operations really out.
NoLostUpdate == Drained => mem.count = out                                   \* the count says what is out

\* The IO object goes only once no operation of the source is out on either ring: none held by the kernel, none posted
\* and unprocessed, none being counted down.
FreedAfterOps == freed => out = 0                                            \* freed means every operation is over

\* `Termination` (every process ends, so a retire or re-home that waited was woken, and the retire let the object go)
\* is the one the translator writes for the fair processes.

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A retire that sleeps is woken by a completion's op_done.
NoOwnerWake == ~ownerWoke                                                    \* never woken by op_done
\* A retire finds operations out and sleeps.
NoRetireSleep == pc[Retirer] # "R_sleep"                                     \* never asleep
\* A re-home finds an operation still out on ring A, waits, and is woken by ring A's op_done.
NoRehomeWait == ~rehomeWaited                                                \* never waited
=============================================================================

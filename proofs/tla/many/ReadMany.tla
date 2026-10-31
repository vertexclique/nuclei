------------------------------ MODULE ReadMany ------------------------------
(***************************************************************************)
(* Many positional reads in one call (src/fs/file/many.rs:                 *)
(* `FileIo::read_many_at`; src/fs/file/positional.rs: `poll_ordered`), at  *)
(* the grain of the call's window and the kernel's ends.                   *)
(*                                                                          *)
(* In plain words: a task asks for N reads of a file, each at an offset of *)
(* its own, in one call. A write at the file's cursor was accepted before  *)
(* the call and has not landed yet. Two parties act: the call (the task's  *)
(* future) and the kernel (the ring of the task's shard, or the blocking   *)
(* pool). The call first waits until the write before it landed, so every  *)
(* read sees it. Then it sends reads out, at most Cap at once (its         *)
(* window), each read exactly once and in the order the task named them.   *)
(* The kernel makes the reads out in any order. Each time one ends, the    *)
(* call takes its result into the place of that read, and sends the next  *)
(* one out. The call is done once every read ended and was taken.          *)
(*                                                                          *)
(* What must never happen: more than Cap reads out at once (`WithinCap`);  *)
(* a read's result given back at another read's place, or a read that did  *)
(* not see the write before the call (`ResultInPlace`); a read sent out    *)
(* twice (`EachOnce`); a call that ends with a read not answered           *)
(* (`AllAnswered`). What must happen: both parties end (`Termination`).    *)
(*                                                                          *)
(* What is abstract: a read moves one cell; the caller's buffers are the  *)
(* results; the ring's own room is the kernel's (it only delays a read).  *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    N,         \* how many reads the call asks for
    Cap,       \* how many reads of the call may be out at once (`MANY_AT_ONCE`)
    Mutation   \* "none", or the name of the one rule this run takes away

\* The reads of the call, numbered in the order the task named them.
Reads == 1..N
\* The cells of the file.
Cells == 0..2
\* Where each read reads: read i reads cell (i - 1) mod 3, so reads 1 and 4 read cell 0.
Off == [i \in Reads |-> (i - 1) % 3]
\* No value yet.
None == -1
\* What the write accepted before the call puts in cell 0.
Written == 7
\* What a read must give back: the write's value in cell 0, else what the cell held all along.
Expected(i) == IF Off[i] = 0 THEN Written ELSE 10 + Off[i]

(* --algorithm ReadMany
variables
    file = [c \in Cells |-> 10 + c],   \* what the file holds: cell c holds 10 + c before the write
    pendingWrite = TRUE,               \* the cursor write accepted before the call has not landed
    next = 1,                          \* the next read of the call to send out
    inflight = {},                     \* the reads out now, not taken back yet
    done = [i \in Reads |-> FALSE],    \* which reads the kernel made
    val = [i \in Reads |-> None],      \* what each read read, once the kernel made it
    results = [i \in Reads |-> None],  \* what the call gives back at each place
    taken = 0,                         \* how many ends the call took back
    sub = [i \in Reads |-> 0],         \* how many times each read went out
    outOfOrder = FALSE;                \* (record) a read ended while one named before it was still out

define
    \* Never more reads out at once than the window holds.
    WithinCap == Cardinality(inflight) <= Cap
    \* Each place holds its own read's result, and that result saw the write before the call.
    ResultInPlace == \A i \in Reads : results[i] # None => results[i] = Expected(i)
    \* No read goes out twice.
    EachOnce == \A i \in Reads : sub[i] <= 1
    \* A call that is done answered every read.
    AllAnswered == pc["call"] = "Done" => \A i \in Reads : results[i] # None
    \* Witness: a read can end before one named before it.
    NoOutOfOrder == ~outOfOrder
end define;

\* The call: `read_many_at`'s future.
fair process call = "call"
begin
C_order:
    \* It waits until the cursor write accepted before it landed (`poll_ordered`).
    \* MUTATION skips_order: the reads go out at once, the write still on its way.
    await ~pendingWrite \/ Mutation = "skips_order";
C_fill:
    \* It sends reads out while some are left and the window has room, one per step.
    \* MUTATION no_cap: it sends every read out, whatever the window holds.
    while next <= N /\ (Cardinality(inflight) < Cap \/ Mutation = "no_cap") do
        \* The next read goes out.
        inflight := inflight \cup {next};
        \* It is counted, to see that it goes out once.
        sub[next] := sub[next] + 1;
        \* MUTATION no_advance: the call does not move on to the next read.
        if Mutation # "no_advance" then
            \* The read after it is the next one.
            next := next + 1;
        end if;
    end while;
C_take:
    \* With nothing out: done once every read went out, else it fills the window again.
    if inflight = {} then
        \* Every read went out and was taken back: the call ends.
        if next > N then
            \* The call returns its results.
            goto Done;
        else
            \* Reads are left: back to sending them out.
            goto C_fill;
        end if;
    end if;
C_wait:
    \* It waits until the kernel made one of the reads out (the read's end wakes it).
    await \E i \in inflight : done[i];
    \* It takes one read that ended.
    with i \in {j \in inflight : done[j]} do
        \* The read is no longer out.
        inflight := inflight \ {i};
        \* MUTATION in_end_order: a result goes to the next free place, in the order reads end.
        if Mutation = "in_end_order" then
            \* The broken call: the place is the count of ends taken so far.
            results[taken + 1] := val[i];
        else
            \* The real call: the result goes to its own read's place (`ends[i]`).
            results[i] := val[i];
        end if;
        \* One more end was taken.
        taken := taken + 1;
    end with;
    \* With room in the window again, the next read goes out.
    goto C_fill;
end process;

\* The kernel: it lands the write before the call, and makes the reads out in any order.
fair process kernel = "kernel"
begin
K_loop:
    \* It works while the call is not done.
    while pc["call"] # "Done" do
        either
            \* The cursor write accepted before the call lands.
            await pendingWrite;
            \* Cell 0 holds the write's value.
            file[0] := Written;
            \* Nothing waits behind it any more.
            pendingWrite := FALSE;
        or
            \* A read out and not made yet is made.
            with i \in {j \in inflight : ~done[j]} do
                \* It is recorded if a read named before it is still out.
                if \E k \in inflight : k < i /\ ~done[k] then
                    \* A read ended out of the order the task named them.
                    outOfOrder := TRUE;
                end if;
                \* The read reads what its cell holds now.
                val[i] := file[Off[i]];
                \* The read is made.
                done[i] := TRUE;
            end with;
        end either;
    end while;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

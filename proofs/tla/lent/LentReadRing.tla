--------------------------- MODULE LentReadRing ---------------------------
(***************************************************************************)
(* A stream read that lends the side the task's own buffer, on a ring      *)
(* (src/io/source/read.rs: `read_owned`, `lent_idle`; src/io/source/read/  *)
(* ops.rs: `submit_ops`, `op_done`).                                       *)
(*                                                                          *)
(* In plain words: on a ring a read is an operation the kernel completes   *)
(* later, and a side has one out at a time. A plain read asks for one,     *)
(* into a buffer of the pool, and its task goes away (its future dropped)  *)
(* while it is out. Then a task reads with its own buffer, again and again *)
(* until the stream ends: it lends the buffer, and since an operation is   *)
(* out already, none goes out for the buffer. The operation out completes  *)
(* into the read-ahead. The lender must be woken for it and take its       *)
(* buffer back to copy the bytes in; else nobody ever reads them, the next *)
(* operation never goes out, and the lender waits forever.                 *)
(*                                                                          *)
(* What is abstract: the side's guard (a step under it runs whole here;    *)
(* Side.tla checks it), and how much one operation takes (all the kernel   *)
(* holds, up to the room of the buffer it reads into).                     *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    Total,     \* the bytes that arrive, numbered 1..Total
    Cap,       \* the room of the lender's buffer
    Mutation   \* "none", or the name of the one rule this run takes away

(* --algorithm LentReadRing
variables
    holder = "none",      \* who holds the side: nobody, the ring's thread or a task
    kernel = <<>>,        \* the bytes the kernel holds, oldest first
    arrived = 0,          \* how many bytes have arrived so far
    closed = FALSE,       \* the peer ended the stream
    out = "none",         \* the side's operation out: none, one into the pool, or one into the lent buffer
    ahead = <<>>,         \* the read-ahead: bytes an operation into the pool brought
    eof = FALSE,          \* the end of the stream, read ahead
    lent = "none",        \* the lender's buffer with the side: none, waiting for bytes, or done
    inLent = <<>>,        \* the bytes the ring read into the lent buffer
    endInLent = FALSE,    \* the ring read the end into the lent buffer
    bufAt = "task",       \* who has the lender's buffer: the task, the side, or the ring
    lenderWoken = FALSE,  \* a wake for the lender is pending
    got = <<>>,           \* the bytes the lender took, in order
    ended = FALSE,        \* the lender read the end
    scribbled = FALSE,    \* (record) the ring read into a buffer it was not lent
    tookAhead = FALSE;    \* (record) the lender's buffer came back for bytes read ahead

define
    \* The smaller of two numbers.
    Min(a, b) == IF a < b THEN a ELSE b
end define;

\* What a poll asks of the ring (`submit_ops`): one operation at a time, and none while a
\* result waits to be taken; into the lent buffer while one waits, else into the pool.
macro Submit() begin
    \* Only with nothing out, nothing read ahead, and no filled buffer waiting.
    if out = "none" /\ ahead = <<>> /\ ~eof /\ lent # "done" then
        \* A buffer that waits goes into the operation; else the pool's does.
        if lent = "waiting" then
            \* The lent buffer moves into the operation.
            out := "lent";
            \* The ring has it now.
            bufAt := "ring";
        else
            \* An operation into a buffer of the pool.
            out := "pool";
        end if;
    end if;
end macro;

\* The peer: its bytes arrive one by one, then it ends the stream.
fair process kernelP = "kernel"
begin
K_arrive:
    \* One byte at a time, numbered in order.
    while arrived < Total do
        \* The next byte arrives.
        arrived := arrived + 1;
        \* The kernel keeps it until an operation takes it.
        kernel := Append(kernel, arrived);
    end while;
K_close:
    \* The peer ends the stream.
    closed := TRUE;
end process;

\* The ring's thread: completes the operation out once the kernel has bytes or the end.
fair process ring = "ring"
begin
G_loop:
    \* Until the lender is done and nothing is out.
    while ~(pc["lender"] = "Done" /\ out = "none") do
        \* An operation out completes once there is something for it.
        await out # "none" /\ (kernel # <<>> \/ closed);
G_hold:
        \* The completion is applied under the side's guard.
        await holder = "none";
        \* The ring's thread holds the side.
        holder := "ring";
G_done:
        \* An operation into the pool fills the read-ahead.
        if out = "pool" then
            \* Bytes, if the kernel has some.
            if kernel # <<>> then
                \* All of them go to the read-ahead.
                ahead := kernel;
                \* The kernel holds none now.
                kernel := <<>>;
            else
                \* Else the end.
                eof := TRUE;
            end if;
            \* A lent buffer that waits is woken for what came (`op_done`).
            \* MUTATION wakes_no_lender: only the plain readers' wakers are woken.
            if lent = "waiting" /\ Mutation # "wakes_no_lender" then
                \* The lender will look again.
                lenderWoken := TRUE;
            end if;
        else
            \* An operation into the lent buffer: the ring may read only into a buffer it has.
            if bufAt # "ring" then scribbled := TRUE; end if;
            \* Bytes, up to the buffer's room, if the kernel has some.
            if kernel # <<>> then
                \* The first bytes, as many as fit.
                inLent := SubSeq(kernel, 1, Min(Cap, Len(kernel)));
                \* The rest stays in the kernel.
                kernel := SubSeq(kernel, Min(Cap, Len(kernel)) + 1, Len(kernel));
            else
                \* Else the end, a read of nothing.
                endInLent := TRUE;
            end if;
            \* The buffer is filled, and back with the side.
            lent := "done";
            \* The side has the buffer again.
            bufAt := "side";
            \* Its task is woken.
            lenderWoken := TRUE;
        end if;
        \* The operation is over.
        out := "none";
        \* The ring's thread lets go of the side.
        holder := "none";
    end while;
end process;

\* A plain read that asks for an operation, then goes away while it is out.
fair process dropped = "dropped"
begin
B_hold:
    \* It takes the side.
    await holder = "none";
    \* It holds the side.
    holder := "dropped";
B_poll:
    \* Its one poll asks for an operation into the pool.
    Submit();
    \* It lets go; its future is dropped, and the operation stays out.
    holder := "none";
end process;

\* A task reading with its own buffer until the stream ends.
fair process lender = "lender"
begin
L_loop:
    \* Read after read, until one reads the end.
    while ~ended do
L_hold:
        \* It takes the side.
        await holder = "none";
        \* It holds the side.
        holder := "lender";
L_take:
        \* A buffer lent that waits, while bytes or the end came to the read-ahead, comes back
        \* to take them (`read_owned` with `lent_idle`).
        \* MUTATION skips_ahead: the waiting buffer stays lent, whatever was read ahead.
        if lent = "waiting" /\ bufAt = "side" /\ (ahead # <<>> \/ eof) /\ Mutation # "skips_ahead" then
            \* The buffer is the task's again.
            lent := "none";
            \* The task has it in hand.
            bufAt := "task";
            \* Record that this path was taken.
            tookAhead := TRUE;
        end if;
L_step:
        \* With its buffer in hand the task takes what was read ahead first.
        if lent = "none" /\ ahead # <<>> then
            \* As many bytes as its buffer holds.
            got := got \o SubSeq(ahead, 1, Min(Cap, Len(ahead)));
            \* The rest stays read ahead.
            ahead := SubSeq(ahead, Min(Cap, Len(ahead)) + 1, Len(ahead));
            \* It lets go; this read is over.
            holder := "none";
        elsif lent = "none" /\ eof then
            \* It takes the end.
            eof := FALSE;
            \* Its reads are over.
            ended := TRUE;
            \* It lets go.
            holder := "none";
        elsif lent = "done" then
            \* Its buffer came back filled: it takes the bytes.
            got := got \o inLent;
            \* The buffer holds none now.
            inLent := <<>>;
            \* A read of nothing is the end.
            ended := endInLent;
            \* The end was taken.
            endInLent := FALSE;
            \* The buffer is the task's again.
            lent := "none";
            \* The task has it.
            bufAt := "task";
            \* It lets go; this read is over.
            holder := "none";
        else
            \* Nothing for it yet: its buffer is lent, if it was not.
            if lent = "none" then
                \* The side has the buffer.
                lent := "waiting";
                \* It is with the side.
                bufAt := "side";
            end if;
L_submit:
            \* It asks the ring for what it needs, if nothing is out.
            Submit();
            \* It lets go, to wait.
            holder := "none";
L_wait:
            \* It waits for a wake.
            await lenderWoken;
            \* The wake is taken.
            lenderWoken := FALSE;
        end if;
    end while;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* Whether a sequence of byte numbers only goes up.
Increasing(s) == \A i \in 1..(Len(s) - 1) : s[i] < s[i + 1]

\* No byte is lost or made up: each is in the kernel, read ahead, in the lent buffer, or taken.
Conservation == Len(got) + Len(inLent) + Len(ahead) + Len(kernel) = arrived

\* The lender gets the bytes in the order they came.
Order == Increasing(got)

\* The ring reads only into a buffer the side gave it.
NoScribble == ~scribbled

\* Once the lender is done, it took every byte.
AllTaken == pc["lender"] = "Done" => Len(got) = Total

\* `Termination` (every process ends, so the lender read every byte and the end) is the one the
\* translator writes for the fair processes.

(***************************************************************************)
(* Witness: claims an interesting case never happens and must be proved    *)
(* wrong.                                                                  *)
(***************************************************************************)

\* The lender's buffer comes back for bytes the dropped read's operation brought.
NoTookAhead == ~tookAhead
=============================================================================

------------------------------ MODULE PortKeep ------------------------------
(***************************************************************************)
(* A shard's completion port while the shard does not run, and a source  *)
(* that moves off it with operations still out (src/rt/shard/port.rs:     *)
(* `hand_off_port`, `take_port_back`; src/rt/park/iocp.rs: `Port::keep`;  *)
(* src/io/source/port.rs: `attach_port`; src/io/source/ops.rs: `op_done`, *)
(* `waits_for_ops`), at the grain of each atomic access.                  *)
(*                                                                          *)
(* A source whose side another thread holds is evicted by that holder as *)
(* it lets go, and may get one more operation out on the port before: the *)
(* shard waits for the eviction before it looks at what is out.           *)
(*                                                                          *)
(* In plain words: a source has operations out on its shard's port (a TCP *)
(* send, never cancelled, and a receive). The shard stops running: it     *)
(* delivers what the port has ready, and hands the rest to a keeper, a    *)
(* job of the blocking pool that drains the port until none is out. The   *)
(* shard may run again: a keeper that has not started is called off, and *)
(* one that keeps is stopped (a wake ends its wait) and waited for, so one *)
(* thread at most drains the port. Meanwhile the source's task registers *)
(* it with another port. Its handle cannot move while the kernel holds an *)
(* operation of it: the registration then raises a flag and looks at the *)
(* count of operations not delivered again; the delivery of the last one  *)
(* lowers the count, and if the flag is up takes it and has the task look *)
(* again. With the count at zero and the flag taken back, the task binds   *)
(* once more.                                                              *)
(*                                                                          *)
(* With `Duplex` the operation out is that TCP send into a full socket,  *)
(* whose peer reads only once it got what it waits for from us: the send *)
(* ends only after the task's first read, and the send is never          *)
(* cancelled (`may_cancel`). A registration that finds the handle unable  *)
(* to move (`attach_port`) keeps the source on its old port, registered   *)
(* with its new shard (`pending`): its reads go to the old port, whose    *)
(* keeper drains it while a source waits to move (`Port::keep`,           *)
(* `hand_off_port`), and once none of its operations is out the move is   *)
(* made (the flag of `waits_for_ops` has it made again). The task reads    *)
(* twice, so a read after the send ended is served too. Before, the       *)
(* registration waited for the send, which waited for the peer, which     *)
(* waited for the task's read, which waited for the registration: both    *)
(* ends waited for good (`wait_for_move`).                                *)
(*                                                                          *)
(* What is modelled step by step: the kernel's completions, each packet's *)
(* delivery (its dequeue, the source's count and the flag's look as one   *)
(* step, as one thread makes them), the hand-off, the take-back's two     *)
(* compare-exchanges, the keeper's start, loop and end, and every access  *)
(* of the registration's wait. What is abstract: one source, one shard,    *)
(* one keeper, and the other port the task registers with.                 *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Ops,       \* the source's operations out on the port at the start
    Back,      \* TRUE: the shard may run again
    Duplex,    \* TRUE: they are a TCP send the peer takes only after the task's first read
    Mutation   \* "none", or the name of the one rule this run takes away

(* --algorithm PortKeep
variables
    inKernel = Ops,       \* operations the kernel holds
    queued = 0,           \* their packets on the port, not dequeued
    wakes = 0,            \* wake packets on the port
    running = TRUE,       \* the shard runs, and drains its port
    handedOff = FALSE,    \* the shard stopped running and its sources went
    kstate = "none",      \* the keeper: "none", "asked", "keeping", "stopping", "done", "called_off"
    flag = FALSE,         \* a registration waits for the source's operations to end
    looked = FALSE,       \* the delivery of the last one had the task look again
    registered = FALSE,   \* the source is registered with the other port
    failed = FALSE,       \* the registration failed
    waited = FALSE,       \* (record) a registration waited
    stopped = FALSE,      \* (record) a keeper that kept was stopped
    kept = FALSE,         \* (record) the keeper delivered a packet
    evicted = FALSE,      \* the holder of a side performed the source's eviction
    readDone = FALSE,     \* the task's read on the other port completed
    peerRead = FALSE,     \* (Duplex) the peer reads, so the socket has room for the send
    pending = FALSE,      \* (Duplex) the source is registered with its new shard, its operations on the old port
    readIn = 0,           \* (Duplex) the task's read the kernel holds, on the old port
    readQueued = 0,       \* (Duplex) its packet on the old port, not dequeued
    readsDone = 0,        \* (Duplex) the task's reads that completed
    same = FALSE;         \* the registration is with the shard whose port the handle is bound to

define
    \* The source's operations not delivered: the port's count, and the source's.
    Out == inKernel + queued + readIn + readQueued
    \* Whether a drainer of the port stays for it: operations out, or a source that waits to move off it.
    \* MUTATION keeper_leaves: it leaves once none is out, though a source still waits to move.
    Kept == Out > 0 \/ (pending /\ Mutation # "keeper_leaves")
    \* The reads the task makes.
    Reads == IF Duplex THEN 2 ELSE 1
end define;

\* One packet of an operation is dequeued and delivered: the source's count
\* goes down, and the last one's end has the task look again if a
\* registration waits.
macro Deliver(who) begin
    if queued > 0 then
        queued := queued - 1;
    else
        \* The task's read, which it now has.
        readQueued := readQueued - 1;
        readsDone := readsDone + 1;
    end if;
    if who = "keeper" then kept := TRUE; end if;
    \* Out still counts the packet this step dequeued (an operator of the define block reads the
    \* state before the step): one left is the last one, as `op_done`'s `fetch_sub(1) == 1`.
    if Out = 1 /\ flag then
        flag := FALSE;
        looked := TRUE;
    end if;
end macro;

\* The kernel completes each operation, at some point.
fair process kernel = "kernel"
begin
K_loop:
    while TRUE do
        either
            \* A send into a full socket ends only once its peer reads.
            await inKernel > 0 /\ (~Duplex \/ peerRead);
            inKernel := inKernel - 1;
            queued := queued + 1;
        or
            \* The task's read: the peer's bytes are there.
            await readIn > 0;
            readIn := readIn - 1;
            readQueued := readQueued + 1;
        end either;
    end while;
end process;

\* The peer: it waits for the task's bytes before it reads ours.
fair process peer \in (IF Duplex THEN {"peer"} ELSE {})
begin
X_read:
    await readsDone > 0;
    peerRead := TRUE;
end process;

\* A side's holder on another thread: it may hand the port one more
\* operation, then performs the source's eviction as it lets go.
fair process holder = "holder"
begin
H_op:
    either
        skip;
    or
        inKernel := inKernel + 1;
    end either;
H_evict:
    evicted := TRUE;
end process;

fair process shard = "shard"
begin
S_ready:
    \* What the port has ready now is delivered here.
    while queued + readQueued > 0 do
        Deliver("shard");
    end while;
S_evicted:
    \* MUTATION no_evict_wait: the shard looks at what is out before the
    \* eviction a holder performs.
    await evicted \/ Mutation = "no_evict_wait";
S_handoff:
    \* MUTATION no_keeper: the operations still out are left on the port.
    if Kept /\ Mutation # "no_keeper" then
        kstate := "asked";
    end if;
    running := FALSE;
    handedOff := TRUE;
S_away:
    if ~Back then
        goto Done;
    end if;
S_choose:
    either
        goto Done;
    or
        skip;
    end either;
S_call_off:
    \* The shard runs again: a keeper that has not started never will.
    if kstate = "asked" then
        kstate := "called_off";
        goto S_mine;
    end if;
S_stop:
    \* One that keeps is stopped, and a wake ends its wait.
    if kstate = "keeping" then
        kstate := "stopping";
        wakes := wakes + 1;
        stopped := TRUE;
    end if;
S_wait:
    \* MUTATION no_wait: the shard drains its port while the keeper may.
    await kstate # "stopping" \/ Mutation = "no_wait";
S_mine:
    running := TRUE;
S_drain:
    while Kept do
        await queued + readQueued > 0 \/ wakes > 0;
        if queued + readQueued > 0 then
            Deliver("shard");
        else
            wakes := wakes - 1;
        end if;
    end while;
end process;

fair process keeper = "keeper"
begin
P_start:
    await kstate \in {"asked", "called_off"};
    \* MUTATION ignore_called_off: a keeper called off starts all the same.
    if kstate = "asked" \/ Mutation = "ignore_called_off" then
        kstate := "keeping";
    else
        goto Done;
    end if;
P_loop:
    while Kept /\ kstate # "stopping" do
        await queued + readQueued > 0 \/ wakes > 0;
        if queued + readQueued > 0 then
            Deliver("keeper");
        else
            wakes := wakes - 1;
        end if;
    end while;
P_end:
    \* A shard that stopped it waits for this.
    kstate := "done";
end process;

\* The source's task registers it with another port, once its shard let go.
fair process task = "task"
begin
T_bind:
    await handedOff /\ evicted;
    \* The task polls where it runs: on the old shard again, if it runs again, or on another.
    with here \in (IF running THEN {TRUE, FALSE} ELSE {FALSE}) do
        same := here;
    end with;
T_here:
    \* Bound to that shard's port already: nothing moves, and the registration is made at once.
    \* MUTATION no_here: it binds all the same, which fails with an operation of the handle out.
    if same /\ Mutation # "no_here" then
        registered := TRUE;
        goto T_read;
    \* The bind fails while the kernel holds an operation of the handle.
    elsif inKernel + readIn = 0 then
        registered := TRUE;
        if pending then                                                      \* a move that waited is made:
            pending := FALSE;                                                \* the source is on its new port
            wakes := wakes + 1;                                              \* and a keeper waiting looks again
        end if;
        goto T_read;
    elsif Mutation = "fail_on_ops" then
        \* MUTATION fail_on_ops: a failed bind fails the registration.
        failed := TRUE;
        goto Done;
    end if;
T_pend:
    \* The source stays on its old port, registered with its new shard: its operations go there.
    \* MUTATION wait_for_move: it waits unregistered for its operations to end, as it did.
    if Duplex /\ ~same /\ Mutation # "wait_for_move" then
        pending := TRUE;
    end if;
T_idle:
    if Out = 0 then
        goto T_retry;
    end if;
T_flag:
    flag := TRUE;
T_recheck:
    \* MUTATION no_recheck: the count is not looked at after the flag went up.
    if Out > 0 \/ Mutation = "no_recheck" then
        goto T_wait;
    end if;
T_take:
    if flag then
        flag := FALSE;
        goto T_retry;
    end if;
T_wait:
    waited := TRUE;
T_sleep:
    await looked;
    looked := FALSE;
    \* MUTATION no_rebind: the move is never made again once its operations ended.
    if Mutation = "no_rebind" then
        goto Done;
    else
        goto T_bind;
    end if;
T_retry:
    if inKernel + readIn = 0 then
        registered := TRUE;
        if pending then                                                      \* a move that waited is made
            pending := FALSE;
            wakes := wakes + 1;
        end if;
    else
        failed := TRUE;
    end if;
T_read:
    \* Registered, the task's read goes to the other port and completes.
    if registered then
        readDone := TRUE;
    end if;
end process;

\* (Duplex) The task's reads: on the other port once registered there, on the old one while the source waits to
\* move off it, where a drainer of that port delivers them.
fair process reader \in (IF Duplex THEN {"reader"} ELSE {})
variables n = 0;                                                             \* the reads made
begin
R_read:
    while n < Reads do
        await registered \/ pending;                                         \* the source is reachable
        n := n + 1;
        if registered then
            readsDone := readsDone + 1;                                      \* the new port serves it
        else
            readIn := readIn + 1;                                            \* on the old port
R_wait:
            await readsDone >= n;                                            \* until a drainer delivers it
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

\* One thread at most drains the port: never the shard while a keeper may.
OneDrainer == ~(running /\ kstate \in {"keeping", "stopping"})

\* A registration never fails for operations that were still out.
NoFailure == ~failed

\* Every operation's packet is delivered, whether the shard runs again or
\* not: from some point on none is left out.
AllDelivered == <>[](Out = 0)

\* The source ends registered with the other port.
Registers == <>registered

\* The task's reads are served.
ReadServed == <>(IF Duplex THEN readsDone = Reads ELSE readDone)

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A registration waits for the source's operations to end.
NoWaited == ~waited
\* The shard runs again before its keeper started, and calls it off.
NoCalledOff == kstate # "called_off"
\* The shard runs again while its keeper keeps, and stops it.
NoStopped == ~stopped
\* The keeper delivers a packet.
NoKept == ~kept
=============================================================================

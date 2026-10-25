------------------------------ MODULE PortKeep ------------------------------
(***************************************************************************)
(* A shard's completion port while the shard does not run, and a source  *)
(* that moves off it with operations still out (src/rt/shard/port.rs:     *)
(* `hand_off_port`, `take_port_back`; src/rt/park/iocp.rs: `Port::keep`;  *)
(* src/io/source/port.rs: `attach_port`; src/io/source/ops.rs: `op_done`, *)
(* `waits_for_ops`), at the grain of each atomic access.                  *)
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
    kept = FALSE;         \* (record) the keeper delivered a packet

define
    \* The source's operations not delivered: the port's count, and the source's.
    Out == inKernel + queued
end define;

\* One packet of an operation is dequeued and delivered: the source's count
\* goes down, and the last one's end has the task look again if a
\* registration waits.
macro Deliver(who) begin
    queued := queued - 1;
    if who = "keeper" then kept := TRUE; end if;
    if inKernel + queued = 0 /\ flag then
        flag := FALSE;
        looked := TRUE;
    end if;
end macro;

\* The kernel completes each operation, at some point.
fair process kernel = "kernel"
begin
K_loop:
    while inKernel > 0 do
        inKernel := inKernel - 1;
        queued := queued + 1;
    end while;
end process;

fair process shard = "shard"
begin
S_ready:
    \* What the port has ready now is delivered here.
    while queued > 0 do
        Deliver("shard");
    end while;
S_handoff:
    \* MUTATION no_keeper: the operations still out are left on the port.
    if Out > 0 /\ Mutation # "no_keeper" then
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
    while Out > 0 do
        await queued > 0 \/ wakes > 0;
        if queued > 0 then
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
    while Out > 0 /\ kstate # "stopping" do
        await queued > 0 \/ wakes > 0;
        if queued > 0 then
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
    await handedOff;
    \* The bind fails while the kernel holds an operation of the handle.
    if inKernel = 0 then
        registered := TRUE;
        goto Done;
    elsif Mutation = "fail_on_ops" then
        \* MUTATION fail_on_ops: a failed bind fails the registration.
        failed := TRUE;
        goto Done;
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
    goto T_bind;
T_retry:
    if inKernel = 0 then
        registered := TRUE;
    else
        failed := TRUE;
    end if;
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

\* Every operation's packet is delivered, whether the shard runs again or not.
AllDelivered == <>(Out = 0)

\* The source ends registered with the other port.
Registers == <>registered

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

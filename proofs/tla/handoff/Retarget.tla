------------------------------ MODULE Retarget ------------------------------
(***************************************************************************)
(* A nested `block_on` moving to its thread's own shard after a handoff    *)
(* (src/rt/handoff/, `Signal` in src/rt/shard.rs), on a machine whose      *)
(* stores wait in a store buffer, as x86's do.                             *)
(*                                                                          *)
(* In plain words: the future's waker holds the shard it wakes in a word   *)
(* (its target). When the thread finds its shard taken, it aims the waker  *)
(* at its own shard and then looks at the woken flag once more before it   *)
(* sleeps there; a waker sets the flag and then reads the target and wakes *)
(* that shard. With both pairs sequentially consistent, at least one side  *)
(* sees the other: the thread finds the flag, or the waker wakes the own   *)
(* shard. With relaxed accesses and no fence both reads can be old: the    *)
(* waker wakes the old shard, which runs on another thread now, and the    *)
(* future sleeps on its own shard with nobody to wake it.                  *)
(*                                                                          *)
(* What is modelled step by step: each thread's store buffer, the flush of *)
(* a buffer's oldest store at any time, each store and load of the pair.   *)
(* What is abstract: a wake of a shard is a counter the sleeper waits on.  *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    Mode       \* "seqcst" or "relaxed"

Mover == 1
Waker == 2
Flusher == 0
Threads == {Mover, Waker}
Words == {"target", "woken"}
OLD == 1
OWN == 2

SeqCst == Mode = "seqcst"

Newest(s, v) == {i \in DOMAIN s : s[i][1] = v /\ \A j \in DOMAIN s : s[j][1] = v => j <= i}
Read(mem, buf, t, v) ==
    IF Newest(buf[t], v) = {} THEN mem[v] ELSE buf[t][CHOOSE i \in Newest(buf[t], v) : TRUE][2]

(* --algorithm Retarget
variables
    mem = [target |-> OLD, woken |-> 0],
    buf = [t \in Threads |-> <<>>],
    wakesOld = 0,
    wakesOwn = 0,
    polled = FALSE,
    woke = FALSE;

define
    \* A wake that came is answered by a poll of the future.
    WokenPolled == woke ~> polled
    \* Witness.
    NoOldWake == wakesOld = 0
end define;

fair process flusher = Flusher
begin
Flush:
    while TRUE do
        with t \in {u \in Threads : buf[u] # <<>>} do
            mem[Head(buf[t])[1]] := Head(buf[t])[2];
            buf[t] := Tail(buf[t]);
        end with;
    end while;
end process;

fair process mover = Mover
begin
M_aim:
    \* The waker's target := the own shard (a swap: sequentially consistent).
    if SeqCst then
        await buf[Mover] = <<>>;
        mem["target"] := OWN;
    else
        buf[Mover] := Append(buf[Mover], <<"target", OWN>>);
    end if;
M_look:
    if Read(mem, buf, Mover, "woken") = 1 then
        polled := TRUE;
        goto Done;
    end if;
M_sleep:
    \* Asleep on the own shard until it is woken.
    await wakesOwn > 0;
    polled := TRUE;
end process;

fair process waker = Waker
variables aim = 0;
begin
K_set:
    woke := TRUE;
    if SeqCst then
        await buf[Waker] = <<>>;
        mem["woken"] := 1;
    else
        buf[Waker] := Append(buf[Waker], <<"woken", 1>>);
    end if;
K_look:
    aim := Read(mem, buf, Waker, "target");
K_wake:
    if aim = OWN then
        wakesOwn := wakesOwn + 1;
    else
        wakesOld := wakesOld + 1;
    end if;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

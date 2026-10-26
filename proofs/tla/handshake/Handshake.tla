----------------------------- MODULE Handshake -----------------------------
(***************************************************************************)
(* The remote wake handshake (src/rt/handshake.rs, its callers in          *)
(* src/rt/shard.rs, measured in src/syscore/linux/bench/wake_handshake.rs) *)
(* on a machine whose stores wait in a store buffer, as x86's do: the one  *)
(* reordering of the Dekker pair a weaker ordering lets through is a store *)
(* passed by a later load of another word.                                 *)
(*                                                                          *)
(* In plain words: a shard with nothing to run publishes that it parks and *)
(* then looks at its inbox once more; a waker pushes its work and then     *)
(* looks whether the shard parks, and if so swaps the flag back and        *)
(* notifies it. With every access of the pair sequentially consistent (a   *)
(* store that waits for its buffer to drain, a push that is a locked read- *)
(* modify-write), or with relaxed accesses and a sequentially consistent  *)
(* fence on both sides between the store and the load, at least one side  *)
(* sees the other: the shard finds the work, or the waker finds it parked. *)
(* Relaxed accesses with no fence, or a fence on one side only, let both   *)
(* loads read the old values while both stores still wait in their        *)
(* buffers: the shard sleeps and nobody notifies it.                        *)
(*                                                                          *)
(* What is modelled step by step: each thread's store buffer (in order),   *)
(* the memory, the flush of a buffer's oldest store at any time, each      *)
(* store, load, fence and swap of the pair. What is abstract: the inbox is *)
(* one word, the notification a flag the sleeping shard waits for.        *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    Wakers,    \* how many threads give the shard work at once
    Mode       \* "seqcst", "fenced", "relaxed", "shard_fence_only" or "waker_fence_only"

Shard == 1
WakerIds == 2..(1 + Wakers)
Threads == {Shard} \cup WakerIds
Flusher == 0
Words == {"parked", "work"}

\* A store of the pair waits for its thread's buffer to drain (an exchange, or
\* a locked read-modify-write); else it goes into the buffer.
SeqCstStores == Mode = "seqcst"
ShardFences == Mode \in {"fenced", "shard_fence_only"}
WakerFences == Mode \in {"fenced", "waker_fence_only"}

\* What thread `t` reads of word `v`: its own newest buffered store, else memory.
Newest(s, v) == {i \in DOMAIN s : s[i][1] = v /\ \A j \in DOMAIN s : s[j][1] = v => j <= i}
Read(mem, buf, t, v) ==
    IF Newest(buf[t], v) = {} THEN mem[v] ELSE buf[t][CHOOSE i \in Newest(buf[t], v) : TRUE][2]

(* --algorithm Handshake
variables
    mem = [v \in Words |-> 0],
    buf = [t \in Threads |-> <<>>],
    notified = 0,           \* notifications the shard got
    shardSaw = FALSE,       \* (record) the shard's look found the work
    wakerSaw = FALSE;       \* (record) a waker's look found the shard parked

define
    AllEnd == <>(\A t \in Threads : pc[t] = "Done")
    \* A burst of wakes notifies once.
    OneNotify == notified <= 1
    \* Witnesses.
    NotBothSee == ~(shardSaw /\ wakerSaw)
    NoNotify == notified = 0
end define;

\* The memory: a buffer's oldest store reaches memory, at any time.
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

fair process shard = Shard
variables look = 0;
begin
S_store:
    \* `begin_park`: parked := true.
    if SeqCstStores then
        await buf[Shard] = <<>>;
        mem["parked"] := 1;
    else
        buf[Shard] := Append(buf[Shard], <<"parked", 1>>);
    end if;
S_fence:
    if ShardFences then
        await buf[Shard] = <<>>;
    end if;
S_look:
    \* The look at the inbox once more.
    look := Read(mem, buf, Shard, "work");
    if look = 1 then
        shardSaw := TRUE;
        goto Done;
    end if;
S_sleep:
    \* Asleep until notified.
    await notified > 0;
end process;

fair process waker \in WakerIds
variables seen = 0, old = 0;
begin
W_push:
    \* The push of the work.
    if SeqCstStores then
        await buf[self] = <<>>;
        mem["work"] := 1;
    else
        buf[self] := Append(buf[self], <<"work", 1>>);
    end if;
W_fence:
    if WakerFences then
        await buf[self] = <<>>;
    end if;
W_look:
    \* `wake`: whether the shard parks.
    seen := Read(mem, buf, self, "parked");
    if seen = 0 then
        goto Done;
    end if;
W_swap:
    \* The swap back, a locked read-modify-write: the first waker of a burst notifies.
    await buf[self] = <<>>;
    wakerSaw := TRUE;
    old := mem["parked"];
    mem["parked"] := 0;
    if old = 1 then
        notified := notified + 1;
    end if;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

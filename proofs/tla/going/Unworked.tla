------------------------------- MODULE Unworked -------------------------------
(***************************************************************************)
(* A linger handed to a runtime that has no worker taking tasks            *)
(* (src/rt/runtime.rs: `spawn_linger`; src/rt/linger.rs:                   *)
(* `Inner::take_linger`, `Inner::route_linger`, `Guarded`, `OnThread`,     *)
(* `alone`, `linger_thread`, `Lingering::push`, `Lingering::end`, `RunAll`;*)
(* src/rt/shard/going.rs: `Shard::leave`).                                 *)
(*                                                                          *)
(* Who acts. A thread H that drops a handle whose write-behind holds bytes,*)
(* and so hands the runtime its linger (the drain of those bytes), with no *)
(* worker of the runtime taking tasks (none was built, or every one stops).*)
(* H may run its own shard again (a later `block_on`), or never. The       *)
(* linger thread L, one for the process, which runs every linger it is     *)
(* given. The runtime's drop D, which may come (`Drops`), or never (a      *)
(* runtime that lives as long as the process, the default one among them).*)
(*                                                                          *)
(* What can happen. While the drop has not begun, a linger no worker takes *)
(* goes to the linger thread, still counted by its runtime until it ends,  *)
(* so a drop that comes waits for it. Once the drop began it goes to the   *)
(* runtime's list, which the drop runs; once the drop is over, to the     *)
(* linger thread, counted no more. Before the fix it went to H's own       *)
(* shard, which runs only at H's next `block_on`, or (H having no shard)   *)
(* to the list, which only a drop runs.                                    *)
(*                                                                          *)
(* What must never happen. A byte the handle took never reaches the peer   *)
(* (`AllSent`). The drop returns before every byte reached it              *)
(* (`DropWaits`).                                                          *)
(*                                                                          *)
(* What is abstract. One handle and its linger; the bytes are counted; the *)
(* peer reads (one that does not is the linger's own deadline,            *)
(* close/Close.tla). Whether H runs a shard of its own as it hands the     *)
(* linger is a free choice. tla/going/Linger.tla checks the drop with a    *)
(* worker, a write still out and a stopped inbox.                          *)
(***************************************************************************)
EXTENDS Integers, TLC                                                        \* numbers and TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Drops,      \* TRUE: the runtime is dropped at some moment
    Mutation    \* "none", or the name of the one rule this run takes away

Total == 2                                                                   \* the bytes in the handle's write-behind

(* --algorithm Unworked                                                      \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    linger = "none",   \* where the linger is: "none", "own" (H's shard), "list", "thread" (the linger thread), "done"
    counted = FALSE,   \* the runtime counts the linger (`Lingering::active`)
    sent = 0,          \* bytes the peer got
    closing = FALSE,   \* the runtime's drop began
    over = FALSE,      \* the runtime's drop is over
    dropDone = FALSE,  \* the runtime's drop returned
    early = FALSE;     \* the linger was handed before the drop began

define                                                                       \* names the steps and properties use:
    \* One more byte sent: the linger is done once the last went.
    After == IF sent + 1 = Total THEN "done" ELSE linger                     \* where it is, or done
    \* Every byte reaches the peer in the end.
    AllSent == <>(sent = Total)                                              \* the peer gets them all
    \* When the drop returns, every byte of a linger handed before it began reached the peer.
    DropWaits == (dropDone /\ early) => sent = Total                         \* nothing left behind
    \* Witness: a drop returns after it waited for a linger handed before it began.
    NoWaited == ~(dropDone /\ early)                                          \* claims to: never
end define;                                                                  \* end of the names

\* One byte of the linger sent where it runs; the last one ends it, and its count.
macro Send() begin                                                           \* a step of the drain
    counted := IF sent + 1 = Total THEN FALSE ELSE counted;                  \* `Lingering::ended` at its end
    linger := After;                                                         \* done at the last byte
    sent := sent + 1;                                                        \* the peer gets it
end macro;                                                                   \* end of the macro

\* H drops the handle and hands its linger (`hand`, `take_linger`, `route_linger`, `spawn_linger`).
fair process handing = "H"                                                   \* weakly fair: it drops the handle
begin                                                                        \* its one step:
H_drop:                                                                      \* the handle's drop hands the linger
    if over then                                                             \* the drop is over: alone, counted no more
        linger := "thread";                                                  \* the linger thread runs it
    elsif closing then                                                       \* the drop began: the list it runs
        linger := "list";                                                    \* `Lingering::push`
        counted := TRUE;                                                     \* counted until it ends
    else                                                                     \* the runtime runs, no worker takes it:
        with own \in BOOLEAN do                                              \* H runs (or made) a shard of its own, or not
            \* MUTATION own_shard: on H's own shard, which runs it at H's next `block_on`, as before the fix.
            \* MUTATION list_first: H with no shard: into the list, which only a drop runs, as before the fix.
            linger := IF own /\ Mutation = "own_shard" THEN "own"            \* (mutated) H's own shard
                      ELSE IF ~own /\ Mutation = "list_first" THEN "list"    \* (mutated) the list
                      ELSE "thread";                                         \* the linger thread
        end with;                                                            \* one of them
        \* MUTATION thread_uncounted: the runtime stops counting a linger it sends to the linger thread.
        counted := Mutation # "thread_uncounted";                            \* counted until it ends
        early := TRUE;                                                       \* (record) before the drop
    end if;                                                                  \* handed
end process;                                                                 \* end of H

\* H's next `block_on`, which runs its own shard: it may come, or never (no fairness).
process again = "A"                                                          \* not fair: H may never run its shard again
begin                                                                        \* its steps:
A_run:                                                                       \* H's shard runs the linger
    await linger = "own" \/ (pc["H"] = "Done" /\ linger # "own");          \* one to run, or none ever
    if linger = "own" then Send(); goto A_run; end if;                       \* one byte more, then again
end process;                                                                 \* end of H's next run

\* The linger thread (`linger_thread`): it runs what it is given.
fair process thread = "L"                                                    \* weakly fair: it runs
begin                                                                        \* its steps:
L_run:                                                                       \* a byte of the linger it holds
    await linger = "thread"                                                  \* one to run,
          \/ (pc["H"] = "Done" /\ pc["D"] = "Done" /\ linger # "thread");    \* or none can come
    if linger = "thread" then Send(); goto L_run; end if;                    \* one byte more, then again
end process;                                                                 \* end of the linger thread

\* The runtime's drop, if it comes (`Runtime::drop`, `finish_lingering`).
fair process dropper = "D"                                                   \* weakly fair once it began
begin                                                                        \* its steps:
D_close:                                                                     \* `Lingering::close`
    if ~Drops then goto Done;                                                \* no drop in this run
    else closing := TRUE;                                                    \* lingers go to the list from now on
    end if;                                                                  \* one of the two
D_run:                                                                       \* `RunAll`: the list, until nothing counted is left
    while counted do                                                         \* a linger not ended
        await linger = "list" \/ ~counted;                                   \* one of the list to run, or a wake
        if linger = "list" then Send(); end if;                              \* one byte more
    end while;                                                               \* nothing left
D_end:                                                                       \* `Lingering::end`
    over := TRUE;                                                            \* lingers go alone from now on
    if linger = "list" then                                                  \* one pushed after the run's last look:
        linger := "thread";                                                  \* `run_rest_alone`: the linger thread
        counted := FALSE;                                                    \* ended for the runtime
    end if;                                                                  \* none left
    dropDone := TRUE;                                                        \* the drop returns
end process;                                                                 \* end of the drop
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

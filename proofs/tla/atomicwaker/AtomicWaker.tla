----------------------------- MODULE AtomicWaker -----------------------------
(***************************************************************************)
(* The waker cell (src/sync/atomic_waker.rs: `register`,                   *)
(* `register_displacing`, `register_returning` and its `Abandon` guard,    *)
(* `woken_while_registering`, `take`, `wake`), at the grain of each atomic *)
(* access to its state word and its slot.                                  *)
(*                                                                          *)
(* In plain words: one state word says whether a registration (REGISTERING) *)
(* or a wake (WAKING) holds the slot. A registration claims the word from  *)
(* WAITING with a compare-and-swap (failing, the cell is in use: the task  *)
(* wakes its own waker and polls again), stores its waker unless the slot  *)
(* already wakes the same task (keeping the different one it replaced),    *)
(* and gives the word back with a second compare-and-swap. That one fails  *)
(* only if a wake raised WAKING meanwhile: the registration then takes the *)
(* slot back out, frees the word, and wakes its own waker and the one it   *)
(* replaced. A take raises WAKING; over WAITING it takes the slot and      *)
(* lowers the bit, over anything else it takes nothing (the registration   *)
(* in progress wakes itself). A displacing registration, for a cell tasks  *)
(* share, wakes the different waker it replaced rather than drop it.       *)
(*                                                                          *)
(* Who acts: the tasks that wait (each looks at its event, registers, looks *)
(* again and sleeps until its waker is called) and the threads that set   *)
(* the event and wake the cell. What must never happen: a registration and *)
(* a take touching the slot at once; a task asleep for good with its event *)
(* set.                                                                    *)
(*                                                                          *)
(* A waker's clone is user code, and may panic. A task in ClonePanics has   *)
(* its first clone panic while it holds REGISTERING, before the slot is    *)
(* written: a guard then gives the word back as the failure path does (a   *)
(* wake that met the registration has the slot's waker woken), and the    *)
(* panic goes on to the poll's caller, which polls again. Without the      *)
(* guard REGISTERING stays up for good: every later registration finds the *)
(* cell in use and wakes itself at once, and every take takes nothing.     *)
(*                                                                          *)
(* What is abstract: a waker is its task's name, and calling it marks the  *)
(* task woken; the event is one flag.                                      *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Tasks,       \* the tasks that wait on the cell
    Wakers,      \* the threads that set the event and wake the cell, once each
    Displacing,  \* the tasks register with `register_displacing`
    ClonePanics, \* the tasks whose first clone of their waker panics
    Mutation     \* "none", or the name of the one rule this run takes away

None == "none"

(* --algorithm AtomicWaker
variables
    registering = FALSE,       \* the state word's REGISTERING bit
    waking = FALSE,            \* its WAKING bit
    slot = None,               \* the waker in the slot
    ready = FALSE,             \* the event the tasks wait for
    woken = [t \in Tasks |-> FALSE],  \* each task's waker was called, not yet seen
    owed = [t \in Tasks |-> FALSE],   \* (record) the cell owes the task its own wake
    panicked = [t \in Tasks |-> FALSE]; \* the task's one panicking clone happened

define
    \* The labels where a party reads or writes the slot.
    RegSlot == {"A_store", "A_back"}
    \* A registration and a take never touch the slot at once.
    SlotExclusive ==
        ~((\E t \in Tasks : pc[t] \in RegSlot) /\ (\E w \in Wakers : pc[w] = "T_slot"))
    \* A take that took the slot finds only its own bit up when it lowers it, so the
    \* `fetch_and` that lowers it is the same as a store of WAITING.
    TakeLowersOnlyItsBit == \A w \in Wakers : pc[w] = "T_lower" => waking /\ ~registering
    \* A registration that found the cell in use, or that a wake met, has its own waker
    \* woken by the time it returns: the cell's promise, whether or not its caller looks
    \* again after registering.
    SelfWoken == \A t \in Tasks : pc[t] = "P_check" /\ owed[t] => woken[t]
    \* Every task ends once the event is set.
    AllEnd == <>(\A t \in Tasks : pc[t] = "Done")
    \* REGISTERING, once raised, is always lowered again: the cell recovers from every panic.
    Recovers == registering ~> ~registering
end define;

\* A waker is called: its task is told.
macro fire(t) begin
    if t # None then woken[t] := TRUE; end if;
end macro;

fair process task \in Tasks
variables displaced = None;
begin
P_look:
    \* The task's poll looks at its event first.
    if ready then
        goto Done;
    end if;
A_claim:
    \* `register_returning`: the first compare-and-swap, WAITING to REGISTERING.
    if registering \/ waking then
        \* InUse: the caller wakes its own waker and polls again (a displacing one too).
        \* MUTATION no_self_wake: it does not.
        owed[self] := TRUE;
        if Mutation # "no_self_wake" then
            woken[self] := TRUE;
        end if;
        goto P_check;
    else
        registering := TRUE;
    end if;
A_store:
    \* The slot: kept as it is if it wakes this task, else replaced by a clone of the task's
    \* waker (the different one it held kept aside). The clone is user code: for a task in
    \* ClonePanics the first one panics, before the slot is written.
    if slot # self then
        if self \in ClonePanics /\ ~panicked[self] then
            panicked[self] := TRUE;         \* the clone panics, REGISTERING held
            goto A_unwind;
        else
            displaced := slot;              \* the different waker, kept aside
            slot := self;                   \* the clone, stored
        end if;
    end if;
A_release:
    \* The second compare-and-swap, REGISTERING to WAITING: it fails only with WAKING up.
    if ~waking then
        registering := FALSE;
        \* Kept: a displacing registration wakes the different waker it replaced; a plain
        \* one drops it (one task registers at a time).
        if Displacing then
            fire(displaced);
        end if;
        displaced := None;
        goto P_check;
    end if;
A_back:
    \* A wake met the registration: the slot taken back out ...
    \* MUTATION no_wake_taken: ... and its own waker not woken.
    owed[self] := TRUE;
    if Mutation # "no_wake_taken" then
        fire(slot);
    end if;
    slot := None;
A_free:
    \* ... the word freed with one swap, and the waker it replaced woken.
    \* MUTATION failure_path_drops_displaced: that one is dropped instead.
    registering := FALSE;
    waking := FALSE;
    if Mutation # "failure_path_drops_displaced" then
        fire(displaced);
    end if;
    displaced := None;
P_check:
    \* The look again after registering: whatever came before it is seen here.
    owed[self] := FALSE;
    if ready then
        goto Done;
    end if;
P_sleep:
    \* Asleep until its waker is called; then it polls again.
    await woken[self];
    woken[self] := FALSE;
    goto P_look;
A_unwind:
    \* The guard of the panicking clone gives the word back as the failure path does: with no
    \* wake met, one compare-and-swap lowers REGISTERING; with WAKING up, the slot's waker
    \* (the one registered before) is taken out and woken, and the word freed. The slot keeps
    \* what it held. Then the panic reaches the poll's caller, which polls again.
    \* MUTATION clone_unguarded: no guard; REGISTERING stays up for good.
    if Mutation # "clone_unguarded" then
        if waking then
            fire(slot);                     \* the wake that met the registration reaches its task
            slot := None;                   \* the slot handed back empty
            waking := FALSE;                \* the word freed ...
        end if;
        registering := FALSE;               \* ... REGISTERING lowered
    end if;
    goto P_look;
end process;

fair process waker \in Wakers
variables got = None;
begin
W_set:
    ready := TRUE;
T_raise:
    \* `take`: one fetch_or of WAKING; over anything but WAITING it takes nothing.
    if registering \/ waking then
        waking := TRUE;
        goto Done;
    else
        waking := TRUE;
    end if;
T_slot:
    got := slot;
    slot := None;
T_lower:
    \* One fetch_and lowers WAKING, then the waker taken is called (`wake`).
    waking := FALSE;
    fire(got);
    got := None;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

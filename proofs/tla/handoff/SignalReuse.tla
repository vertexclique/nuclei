---------------------------- MODULE SignalReuse ----------------------------
(***************************************************************************)
(* The waker a shard's `block_on` keeps for the next one (src/rt/shard/    *)
(* run.rs: `Shard::signal`, `Shard::keep_signal`, `Shard::block_on` and    *)
(* `finish_on_own`; src/rt/shard/signal.rs: `Signal::reset`, the clone and *)
(* drop of its wakers; src/rt/handoff/target.rs: `Target::move_to`).       *)
(*                                                                          *)
(* Who acts. The shard's runner, which makes `block_on` calls one after    *)
(* another on the shard (whichever thread runs it: a take hands the shard  *)
(* to another thread, which goes on with the next call). An outside       *)
(* holder X, which a call's future may hand a clone of its waker, and     *)
(* which wakes it later, or drops it, whenever it likes.                   *)
(*                                                                          *)
(* What can happen. Each call takes the signal the shard's cell kept, if  *)
(* nothing else holds it (`Arc::strong_count == 1`) and its target never  *)
(* moved (`reset`), else makes a new one. A call whose shard is taken      *)
(* while it runs aims its signal at the old thread's own shard            *)
(* (`finish_on_own`), and keeps it in the cell of the shard it lost no    *)
(* more: its caller looks at the take first (`context::detached_from`).   *)
(* A call that ends on the shard keeps its signal there.                   *)
(*                                                                          *)
(* What must never happen. A waker X kept from one call's future wakes the *)
(* future of a later call (the reuse hands a stale waker on: a spurious    *)
(* poll, which the future's contract allows, but which the count check is *)
(* there to spare); and a call on the shard waits on a signal aimed at     *)
(* another shard (its wakes would wake that other shard, and the call      *)
(* would sleep for good).                                                  *)
(*                                                                          *)
(* What is abstract. Two calls; at most one take, during either; X gets a  *)
(* clone at most once. The cell, the counts and the target are read and    *)
(* written in one step each, sequentially consistent: the acquire fence   *)
(* after the count of one (a holder's drop on another thread, a release,  *)
(* comes before the reuse) is the memory model's, not this model's.       *)
(***************************************************************************)
EXTENDS Integers, TLC                                                        \* numbers and TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Mutation    \* "none", or the names of the rules this run takes away (see the configurations)

Calls == 2                                                                   \* the `block_on` calls the shard makes
Signals == 1..Calls                                                          \* at most one new signal a call

(* --algorithm SignalReuse                                                   \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    cell = 0,                            \* the signal the shard's cell keeps (`Shard::signal`), 0 for none
    made = 0,                            \* signals made so far: their ids are 1..made
    serves = [s \in Signals |-> 0],      \* the call each signal is the waker of now, 0 for none
    moved = [s \in Signals |-> FALSE],   \* the signal's target moved to the old thread's own shard
    xHolds = 0,                          \* the signal X holds a clone of, 0 for none
    xFor = 0,                            \* the call whose future handed X that clone
    handed = FALSE,                      \* X was handed a clone (once at most)
    taken = FALSE,                       \* the shard was taken during a call (once at most)
    staleWake = FALSE,                   \* (record) X's wake reached a later call's future
    wrongShard = FALSE;                  \* (record) a call on the shard waited on a moved signal

define                                                                       \* names the properties use:
    \* A waker kept from one call's future never wakes the future of another call.
    NoStaleWake == ~staleWake                                                \* never a stale wake
    \* A call on the shard never waits on a signal aimed at another shard.
    WakesItsShard == ~wrongShard                                             \* always its own shard
    \* The rules each run keeps or takes away.
    \* MUTATION reuse_held: the cell's signal is reused while X still holds a clone (no count check).
    CountCheck == Mutation \notin {"reuse_held"}                             \* `strong_count == 1`
    \* MUTATION reuse_moved: a moved signal is reused (no look at `moved` in `reset`).
    MovedCheck == Mutation \notin {"reuse_moved", "reuse_moved_kept"}        \* `reset` false once moved
    \* MUTATION keep_detached: a call taken during its run keeps its signal in the lost shard's cell.
    KeepGuard == Mutation \notin {"keep_detached", "reuse_moved_kept"}       \* `detached_from` before `keep_signal`
end define;                                                                  \* end of the names

\* The shard's runner: the calls, one after another.
fair process runner = "R"                                                    \* weakly fair: the calls go on
variables k = 1, sig = 0;                                                    \* the call now, the signal it runs with
begin                                                                        \* the runner's steps:
R_call:                                                                      \* the next call
    while k <= Calls do                                                      \* until both are made
        \* `Shard::signal`: the cell's signal if nothing else holds it and it never moved, else a new one.
        if /\ cell # 0                                                       \* the cell keeps one,
           /\ (xHolds # cell \/ ~CountCheck)                                 \* nothing else holds it (or the mutation),
           /\ (~moved[cell] \/ ~MovedCheck) then                             \* and it never moved (or the mutation):
            sig := cell;                                                     \* reused
        else                                                                 \* else:
            made := made + 1;                                                \* a new signal
            sig := made;                                                     \* is this call's
        end if;                                                              \* the cell is emptied:
        cell := 0;                                                           \* `self.signal.take()`
        serves[sig] := k;                                                    \* the signal serves this call now
        wrongShard := wrongShard \/ moved[sig];                              \* (record) aimed at another shard
R_poll:                                                                      \* the future's poll
        either                                                               \* it hands X a clone of its waker:
            await ~handed;                                                   \* once at most
            handed := TRUE;                                                  \* (recorded)
            xHolds := sig;                                                   \* X holds the clone
            xFor := k;                                                       \* of this call's waker
        or                                                                   \* or it does not:
            skip;                                                            \* nothing
        end either;                                                          \* one of the two
R_end:                                                                       \* the call ends
        either                                                               \* on the shard:
            cell := sig;                                                     \* `keep_signal`
        or                                                                   \* the shard was taken during the call:
            await ~taken;                                                    \* once at most
            taken := TRUE;                                                   \* (recorded)
            moved[sig] := TRUE;                                              \* `finish_on_own`: aimed at the own shard
            if ~KeepGuard then cell := sig; end if;                          \* (the mutation) kept in the lost cell
        end either;                                                          \* one of the two
        serves[sig] := 0;                                                    \* the signal serves no call now
        k := k + 1;                                                          \* the next call
    end while;                                                               \* both made
end process;                                                                 \* end of the runner

\* X, outside: it wakes the clone it holds, or drops it, whenever it likes.
fair process holder = "X"                                                    \* weakly fair: it acts in the end
begin                                                                        \* X's steps:
X_act:                                                                       \* one act
    while TRUE do                                                            \* for as long as it holds one
        await xHolds # 0;                                                    \* it holds a clone
        either                                                               \* it wakes it (`wake_by_ref`):
            staleWake := staleWake \/ (serves[xHolds] # 0 /\ serves[xHolds] # xFor);  \* (record) a later call's
        or                                                                   \* it drops it (or wakes it by value):
            xHolds := 0;                                                     \* the count it held goes
        end either;                                                          \* one of the two
    end while;                                                               \* the next act
end process;                                                                 \* end of X
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

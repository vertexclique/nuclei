----------------------------- MODULE LingerList -----------------------------
(***************************************************************************)
(* The list of lingers a runtime's drop runs, and its end (src/rt/linger.rs:*)
(* `Lingering::push`, `Lingering::end`, `run_rest_alone`, `RunAll::poll`;  *)
(* src/rt/runtime.rs: `finish_lingering`, which runs `RunAll` within its   *)
(* bound and then calls `end`).                                            *)
(*                                                                          *)
(* Who acts. A thread P that hands the runtime a linger after its drop     *)
(* began (`route_linger` once `closing`): it pushes the linger on the      *)
(* list, wakes the drop's run, and looks whether the run is over; if so it *)
(* takes what the list holds and runs it alone. The drop D: its run        *)
(* (`RunAll`) registers its waker, takes and runs what the list holds, and *)
(* sleeps until woken while a linger is still counted, or until its bound  *)
(* runs out; then `end` says the run is over and takes what the list still *)
(* holds, to run alone.                                                    *)
(*                                                                          *)
(* What can happen. P's push and D's end race. Each side writes, fences,   *)
(* then looks at what the other wrote: P its push, then `over`; D `over`,  *)
(* then the list. Both pairs are sequentially consistent with a fence     *)
(* between, so one side sees the other. And P's wake races the run's      *)
(* sleep: the run registers its waker before it looks, so a push after a  *)
(* look finds the waker there.                                             *)
(*                                                                          *)
(* What must never happen. A linger left in the list once both are done:   *)
(* nobody ever runs it, and the bytes it holds never reach the peer. And  *)
(* the run asleep with a linger in the list and no wake on its way: it     *)
(* sleeps to its bound (a linger's time) while it could run the linger.    *)
(*                                                                          *)
(* What is abstract. One linger, counted from its hand-over               *)
(* (`take_linger`); running it is one step that ends it. The waker is one  *)
(* flag a wake takes (`AtomicWaker`: a wake with none registered is        *)
(* lost). A store buffer is modelled by its effect, as a mutation: one     *)
(* side's look answered before its own store reaches memory.               *)
(***************************************************************************)
EXTENDS Integers, TLC                                                        \* numbers and TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Mutation    \* "none", or the name of the one rule this run takes away

(* --algorithm LingerList                                                    \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    list = 0,            \* lingers in the list (`Lingering::list`)
    over = FALSE,        \* the drop's run is over: lingers go alone (`Lingering::over`)
    active = 1,          \* lingers counted and not ended (`Lingering::active`): P's, from its hand-over
    reg = FALSE,         \* the run's waker is registered (`Lingering::waker`)
    woken = FALSE,       \* the run's waker was woken: it is polled again
    ran = "none",        \* who ran the linger: "none", "run" (the drop's run), "alone" (a thread of its own)
    pLook = FALSE,       \* (push_unfenced) P's look at `over`, answered before its push reached memory
    dLook = 0;           \* (end_unfenced) D's look at the list, answered before its store of `over`

define                                                                       \* names the properties use:
    \* Once P and D are both done, no linger is left in the list.
    NoneLeft == (pc["P"] = "Done" /\ pc["D"] = "Done") => list = 0           \* nothing left behind
    \* The run never sleeps with a linger in the list and no wake on its way.
    NoLostWake == (pc["D"] = "D_sleep" /\ list > 0) => woken                 \* a wake is on its way
    \* Witness: the linger runs alone, after the run is over.
    NoAlone == ran # "alone"                                                 \* claims to: never
end define;                                                                  \* end of the names

\* The waker's wake (`AtomicWaker::wake`): it takes the registered waker and wakes it, if one is there.
macro wake() begin                                                           \* the run is woken
    if reg then reg := FALSE; woken := TRUE; end if;                         \* a wake with none registered is lost
end macro;                                                                   \* end of the wake

\* P, handing the linger after the drop began (`Lingering::push`).
fair process pusher = "P"                                                    \* weakly fair: it goes on
begin                                                                        \* P's steps:
P_push:                                                                      \* its push and wake, then the fence
    \* MUTATION push_unfenced: the look at `over` is answered before the push reaches memory (no fence
    \* between, a store buffer between the two).
    if Mutation = "push_unfenced" then                                       \* the look first:
        pLook := over;                                                       \* answered now
    else                                                                     \* in order:
        list := list + 1;                                                    \* `list.push(work)`
        wake();                                                              \* `waker.wake()`
    end if;                                                                  \* one of the two
P_look:                                                                      \* after the fence, its look at `over`
    if Mutation = "push_unfenced" then                                       \* (the mutation) the push, late:
        list := list + 1;                                                    \* reaches memory now
        wake();                                                              \* and the wake goes
    else                                                                     \* in order:
        pLook := over;                                                       \* the look
    end if;                                                                  \* then the decision:
P_alone:                                                                     \* `run_rest_alone` if the run is over
    if pLook /\ list > 0 then                                                \* over, and the list holds it:
        list := list - 1;                                                    \* taken
        active := active - 1;                                                \* `ended`
        ran := "alone";                                                      \* `alone(work)`: it runs
    end if;                                                                  \* P is done
end process;                                                                 \* end of P

\* D, the drop: `RunAll` within its bound, then `Lingering::end`.
fair process dropper = "D"                                                   \* weakly fair: the drop goes on
begin                                                                        \* D's steps:
D_poll:                                                                      \* a poll of `RunAll`
    \* MUTATION register_late: the waker is registered after the looks, not before.
    if Mutation # "register_late" then reg := TRUE; end if;                  \* before the looks
D_take:                                                                      \* the list taken and run
    if list > 0 then                                                         \* a linger there:
        list := list - 1;                                                    \* taken
        active := active - 1;                                                \* it runs and ends
        ran := "run";                                                        \* (recorded)
    end if;                                                                  \* then the look at the count
D_done:                                                                      \* `lingers_done`
    if Mutation = "register_late" then reg := TRUE; end if;                  \* (the mutation) registered now
    if active = 0 then goto D_end; end if;                                   \* none counted: the run is over
D_sleep:                                                                     \* pending: asleep until woken
    either                                                                   \* woken:
        await woken;                                                         \* the wake came
        woken := FALSE;                                                      \* taken
        goto D_poll;                                                         \* polled again
    or                                                                       \* or the bound runs out:
        skip;                                                                \* the run stops
    end either;                                                              \* one of the two
D_end:                                                                       \* `Lingering::end`: `over`, the fence, the list
    \* MUTATION end_unfenced: the look at the list is answered before the store of `over` reaches memory.
    if Mutation = "end_unfenced" then                                        \* the look first:
        dLook := list;                                                       \* answered now
    else                                                                     \* in order:
        over := TRUE;                                                        \* the store
    end if;                                                                  \* one of the two
D_rest:                                                                      \* after the fence, `run_rest_alone`
    if Mutation = "end_unfenced" then                                        \* (the mutation) the store, late:
        over := TRUE;                                                        \* reaches memory now
    else                                                                     \* in order:
        dLook := list;                                                       \* the look
    end if;                                                                  \* then what it saw:
D_alone:                                                                     \* what the list held, alone
    if dLook > 0 /\ list > 0 then                                            \* it saw one, still there:
        list := list - 1;                                                    \* taken
        active := active - 1;                                                \* `ended`
        ran := "alone";                                                      \* `alone(work)`: it runs
    end if;                                                                  \* the drop returns
end process;                                                                 \* end of D
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

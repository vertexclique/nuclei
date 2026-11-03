------------------------------ MODULE Detached ------------------------------
(***************************************************************************)
(* A thread that waits after the monitor took its shard: the rest of the   *)
(* poll the take found it in calls `blocking_*` or `block_on`, or spawns a *)
(* task (src/sync/internal.rs: `blocking`, `block_on`, `wait_here`;        *)
(* src/rt/runtime.rs: `block_on_here`, `spawn_here`, `lost_spawn`;         *)
(* src/rt/shard/run.rs: `finish_on_own`, `Shard::run_loop`; src/rt/        *)
(* context.rs: `Here::detach`, `make_own_shard`, `aim_home`; src/rt/       *)
(* handoff/home.rs: `Home::push`; src/rt/handoff/target.rs: `Target`;      *)
(* src/rt/workers.rs: `finish_home`; src/coop.rs: `outside`).              *)
(*                                                                          *)
(* Who acts. D, the thread whose shard S the monitor took while D was in   *)
(* the poll of S's task T: D is detached, and T's code runs on. B, the     *)
(* thread that runs S now. R, another thread that wakes U, a task bound to *)
(* D (a `spawn_local` of D's), which only D may run.                       *)
(*                                                                          *)
(* What can happen. T's code may spawn a task V that is `Send` while no    *)
(* worker takes tasks. Then it waits, in a `blocking_*` call or a nested   *)
(* `block_on`, for one of two things: two operations that are ready at    *)
(* once (two values queued on a channel), each spending one of the        *)
(* poll's budget, which may be spent already; or U's end. A wait runs on   *)
(* D's own shard, which D makes if it has none, or (when the own shard     *)
(* cannot start) parks D's thread. R pushes U's runnable to D's home and   *)
(* wakes the shard the home names (its target). After the wait, T's poll   *)
(* ends, and D's bound tasks finish on its own shard (`finish_home`).      *)
(*                                                                          *)
(* What must never happen. The wait never ends: a park whose polls keep    *)
(* the spent budget of T's poll, so every poll refuses and wakes itself    *)
(* (a livelock at full CPU); a park that never runs U, which only D's own  *)
(* shard runs; an own shard asleep with U pushed to the home while the     *)
(* home names S, so the wake goes to B, which never runs D's bound tasks.  *)
(* And V must run: on D's own shard it would wait for D's next `block_on`, *)
(* which may never come.                                                   *)
(*                                                                          *)
(* What is abstract. The take itself (Handoff.tla, HandBack.tla); B's run  *)
(* of S beyond V; the wake handshake of the target, sequentially           *)
(* consistent (Retarget.tla checks its store order); one bound task and    *)
(* one remote wake.                                                        *)
(***************************************************************************)
EXTENDS Integers, TLC                                                        \* numbers, and TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Waits,      \* what T's code waits for: "none", "items" (two ready operations) or "bound" (U's end)
    Bound,      \* TRUE: U, a task bound to D, is live and R wakes it
    Spawn,      \* TRUE: T's code first spawns V while no worker takes tasks
    OwnFails,   \* TRUE: D's own shard cannot start (its poller cannot be made)
    Mutation    \* "none", or the name of the one rule this run takes away

Need == 2                                                                    \* the operations an "items" wait completes
Budget == 1                                                                  \* a poll's budget, in operations
Unbudgeted == -1                                                             \* `coop::UNBUDGETED`: a budget that never runs out

(* --algorithm Detached                                                      \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    ownMade \in (IF OwnFails THEN {FALSE} ELSE BOOLEAN),  \* D's own shard was made before the take, or not
    budget \in 0..Budget,    \* the budget T's poll has left as the take finds it (0: spent)
    target = "S",            \* the shard D's home names to wake (`Home::target`)
    inbox = 0,               \* U's runnables in D's home (0 or 1)
    ownWake = FALSE,         \* a wake of D's own shard waits for its loop
    token = FALSE,           \* D's thread's unpark token (`thread::unpark` before a `park`)
    got = 0,                 \* operations the "items" wait completed
    uRan = FALSE,            \* U ran to its end on D's thread
    vAt = "none",            \* where V went: "none", "S" (S's inbox), "own" (D's own shard)
    vRan = FALSE,            \* V ran
    asleep = FALSE,          \* D's own shard's loop sleeps (parked on its poller)
    waited = FALSE,          \* D's wait returned
    ownInWait = FALSE;       \* (record) a wake of the own shard ended its sleep during the wait

define                                                                       \* names the steps and properties use:
    \* The smaller of two numbers.
    Min(a, b) == IF a < b THEN a ELSE b                                      \* the smaller
    \* Whether the wait's future is ready now.
    Ready == CASE Waits = "items" -> got = Need                              \* its operations done
             []   Waits = "bound" -> uRan                                    \* U ended
             []   OTHER -> TRUE                                              \* no wait at all
    \* The wait returns.
    WaitsEnd == <>waited                                                     \* in the end
    \* U, woken, runs on D's thread.
    BoundRuns == <>uRan                                                      \* in the end
    \* V, spawned, runs.
    SpawnRuns == Spawn => <>vRan                                             \* in the end, if spawned
    \* The own shard never sleeps with U pushed to the home and no wake of it on its way (R's wake
    \* not yet made is on its way).
    NoStrandedBound == (asleep /\ inbox > 0 /\ pc["R"] # "R_wake") => ownWake  \* a wake comes
    \* Witness: a remote wake of U ends the own shard's sleep during the wait.
    NoOwnWakeInWait == ~ownInWait                                            \* claims to: never
end define;                                                                  \* end of the names

\* One poll of the wait's future with budget `b`: an "items" wait completes what `b` lets it (an
\* unbudgeted poll never runs out) and, with some left to do and none to spend, is refused and wakes
\* itself (`coop::spend` refused): `selfWake`. With `spend`, the poll spends the thread's own budget
\* (no fresh one was given). A "bound" wait looks at U alone.
macro PollWait(b, spend, selfWake) begin                                     \* the future's poll
    if Waits = "items" then                                                  \* two ready operations:
        with n = Min(Need - got, IF b = Unbudgeted THEN Need ELSE b) do      \* as many as the budget lets complete
            got := got + n;                                                  \* complete
            if spend then budget := b - n; end if;                           \* spent from the thread's budget
            selfWake := got < Need;                                          \* some refused: the poll wakes itself
        end with;                                                            \* the poll is over
    else                                                                     \* a "bound" wait:
        selfWake := FALSE;                                                   \* never wakes itself
    end if;                                                                  \* the poll's outcome is `Ready`
end macro;                                                                   \* end of the macro

\* D: the rest of T's poll, then its bound tasks.
fair process detached = "D"                                                  \* weakly fair: D runs
variables woken = FALSE;                                                     \* the wait's own wake, for its next poll
begin                                                                        \* D's steps:
D_aim:                                                                       \* `Here::detach`: the take found
    \* An own shard made before the take: the home names it from now on (`aim_home`).
    \* MUTATION home_not_moved: the home's target moves only in `finish_home`, as before the fix.
    if ownMade /\ Mutation # "home_not_moved" then target := "own"; end if;  \* aimed at the own shard
D_spawn:                                                                     \* `spawn` with no worker taking tasks
    \* Fixed: onto the shard the poll lost, whose runner runs it (`lost_spawn`).
    \* MUTATION spawn_own: onto D's own shard, as before the fix.
    if Spawn then vAt := IF Mutation = "spawn_own" THEN "own" ELSE "S"; end if;  \* V queued
D_call:                                                                      \* `blocking` (or `block_on_here`): which wait
    if Waits = "none" then goto D_end;                                       \* no wait: the poll ends
    \* MUTATION park_no_own_shard: no own shard is made for the wait, which parks, as before the fix.
    elsif Mutation = "park_no_own_shard" /\ ~ownMade then goto P_poll;       \* (mutated) the park
    elsif ~ownMade /\ OwnFails then goto P_poll;                             \* no own shard can start: the park
    elsif ~ownMade then                                                      \* `make_own_shard`
        ownMade := TRUE;                                                     \* made now
        if Mutation # "home_not_moved" then target := "own"; end if;         \* and the home aimed at it first
    end if;                                                                  \* the own shard's loop
O_poll:                                                                      \* `run_loop`'s poll of the future
    \* `coop::enter`: a fresh budget for the poll, the thread's put back after it.
    PollWait(Budget, FALSE, woken);                                          \* the poll
O_ready:                                                                     \* its outcome
    if Ready then waited := TRUE; goto D_end; end if;                        \* the wait returns
O_look:                                                                      \* the loop takes a runnable
    if inbox > 0 then                                                        \* U in the home (`pop_home`):
        inbox := 0;                                                          \* taken
        uRan := TRUE;                                                        \* U runs to its end
        woken := TRUE;                                                       \* its end wakes the wait
        goto O_poll;                                                         \* polled next
    elsif vAt = "own" /\ ~vRan then                                          \* V in the own shard's queue:
        vRan := TRUE;                                                        \* V runs
        goto O_look;                                                         \* the next look
    elsif woken then goto O_poll;                                            \* the future woke itself: polled
    end if;                                                                  \* nothing to run and no wake: the park
O_park:                                                                      \* `begin_park`: parked first
    asleep := TRUE;                                                          \* the parked flag
O_back:                                                                      \* then one more look at the work
    if inbox > 0 \/ ownWake then                                             \* work came:
        asleep := FALSE;                                                     \* backs out
        ownWake := FALSE;                                                    \* a wake on its way is taken
        goto O_look;                                                         \* and runs it
    end if;                                                                  \* nothing: it sleeps
O_sleep:                                                                     \* asleep on the poller
    await ownWake;                                                           \* until the own shard is woken
    ownWake := FALSE;                                                        \* the wake is taken
    asleep := FALSE;                                                         \* awake
    ownInWait := TRUE;                                                       \* (record)
    goto O_look;                                                             \* the loop looks again
P_poll:                                                                      \* `wait_here`'s park loop: a poll
    \* Fixed: unbudgeted (`coop::outside`), as a thread that parks runs nothing else.
    \* MUTATION park_keeps_budget: it keeps T's poll's budget, as before the fix.
    if Mutation = "park_keeps_budget" then                                   \* (mutated)
        PollWait(budget, TRUE, token);                                       \* spends the thread's budget
    else                                                                     \* fixed:
        PollWait(Unbudgeted, FALSE, token);                                  \* never refused
    end if;                                                                  \* a refused poll unparked this thread
P_ready:                                                                     \* its outcome
    if Ready then waited := TRUE; goto D_end; end if;                        \* the wait returns
P_park:                                                                      \* `thread::park`
    await token;                                                             \* until unparked
    token := FALSE;                                                          \* the token is taken
    goto P_poll;                                                             \* polled again
D_end:                                                                       \* T's poll ends; `finish_home`
    if Bound /\ ~uRan /\ ~OwnFails then                                      \* a bound task still live:
        ownMade := TRUE;                                                     \* the own shard (made if none)
        target := "own";                                                     \* aimed at it (a second move does nothing)
    else goto Done;                                                          \* none (or no shard can run it): over
    end if;                                                                  \* its loop, until U ended
F_look:                                                                      \* `home_done` on the own shard
    if inbox > 0 then inbox := 0; uRan := TRUE; goto Done;                   \* U runs: done
    elsif vAt = "own" /\ ~vRan then vRan := TRUE; goto F_look;               \* V, queued here, runs too
    end if;                                                                  \* nothing to run: the park
F_park:                                                                      \* parked first
    asleep := TRUE;                                                          \* the parked flag
F_back:                                                                      \* one more look
    if inbox > 0 \/ ownWake then                                             \* work came:
        asleep := FALSE;                                                     \* backs out
        ownWake := FALSE;                                                    \* the wake is taken
        goto F_look;                                                         \* and runs it
    end if;                                                                  \* nothing: it sleeps
F_sleep:                                                                     \* asleep
    await ownWake;                                                           \* until woken
    ownWake := FALSE;                                                        \* taken
    asleep := FALSE;                                                         \* awake
    goto F_look;                                                             \* looks again
end process;                                                                 \* end of D

\* R: a thread that wakes U, bound to D (`Home::push`).
fair process remote = "R"                                                    \* weakly fair: the wake comes
begin                                                                        \* R's steps:
R_push:                                                                      \* the push to D's home
    if ~Bound then goto Done;                                                \* no U: nothing to wake
    else inbox := 1;                                                         \* U's runnable waits there
    end if;                                                                  \* then the wake
R_wake:                                                                      \* then the target's read and its wake
    if target = "own" then ownWake := TRUE; end if;                          \* D's own shard woken; else S, whose runner ignores D's home
end process;                                                                 \* end of R

\* B: the thread that runs S now; it runs S's inbox, never D's home.
fair process runner = "B"                                                    \* weakly fair: B runs
begin                                                                        \* B's steps:
B_run:                                                                       \* a run of S's inbox
    await vAt = "S";                                                         \* V pushed to S
    vRan := TRUE;                                                            \* V runs
end process;                                                                 \* end of B
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

------------------------------- MODULE Sched -------------------------------
(***************************************************************************)
(* The order a shard takes its runnables in (src/rt/shard/order.rs:       *)
(* `Shard::next`, `pop_inbox`, `pop_home`; src/rt/shard/                  *)
(* run.rs: the `owed` rule of `Shard::run_loop`), one turn of the run loop *)
(* per pass.                                                               *)
(*                                                                          *)
(* In plain words: a shard has five places work waits in. The LIFO slot    *)
(* holds the task the running task woke last; the local queue the tasks of *)
(* its own thread, in order; the inbox the tasks other threads woke; the   *)
(* home the tasks bound to the thread that runs the shard, which other     *)
(* threads woke; and a `block_on` future may wait to be polled again. Each *)
(* turn the loop polls the future if it was woken and owes the tasks       *)
(* nothing, then takes one runnable: every `I`th turn first from the       *)
(* inbox (the home if the inbox is empty); otherwise the slot (at most `L` *)
(* times in a row while other tasks wait), then the local queue, then the  *)
(* inbox, then the home. A future that woke itself owes the tasks queued   *)
(* at that moment their runs before its next poll. Other threads may keep *)
(* the inbox and the home full, and the tasks may keep waking each other: *)
(* whatever they do, every place that holds work must be served within    *)
(* `Bound` turns.                                                          *)
(*                                                                          *)
(* The code today looks at the home only once the inbox is empty, so a     *)
(* home starves while other threads keep the inbox full: that is the       *)
(* `home_after_empty` mutation, which breaks `Served`. The GREEN           *)
(* configurations check the proposed fix, which is not in the code: an    *)
(* interval turn that finds work in the inbox, every other time, runs a    *)
(* runnable of the home's instead and puts the inbox's at the front of the *)
(* local queue. Every variant of it measured moves a counted row of the    *)
(* op-count gate (`block_on` by two instructions in the best), so it waits *)
(* for the operator's call.                                                *)
(*                                                                          *)
(* What is abstract: a place is a count of its runnables (their order      *)
(* inside one place is the place's own, FIFO or one slot); a run is what   *)
(* the runnable it took pushes: nothing, a wake of another task (to the    *)
(* slot, the slot's runnable before it to the local queue's back), or a    *)
(* yield of its own (to the local queue's back).                           *)
(***************************************************************************)
EXTENDS Integers

CONSTANTS
    I,         \* the inbox interval (`INBOX_INTERVAL`, `Builder::global_queue_interval`)
    L,         \* `LIFO_MAX`: runs of the slot in a row while others wait
    Cap,       \* the most runnables the model's local queue holds
    Future,    \* TRUE: a `block_on` future runs the loop, and may be woken any time
    Mutation   \* "none", or the name of the one rule this run takes away

\* The places work waits in.
Places == {"lifo", "local", "inbox", "home", "future"}
\* Within this many turns every place that holds work is served: the inbox and the home wait
\* up to two interval turns (the home's and the inbox's alternate); the slot, the local queue
\* and the future wait less, with the configurations' constants.
Bound == 2 * I - 1

(* --algorithm Sched
variables
    lifo = 0,           \* the LIFO slot holds a runnable (1) or not (0)
    streak = 0,         \* runs of the slot in a row (`lifo_streak`)
    local = 0,          \* runnables in the local queue (at most Cap + 1)
    inbox = 0,          \* the inbox holds runnables (1) or not (0)
    home = 0,           \* the home holds runnables (1) or not (0)
    until = I,          \* turns left to the interval's turn (`until_inbox`)
    homesFirst = FALSE, \* the last such turn with inbox work ran the home's (the fix)
    woken = FALSE,      \* the future was woken
    owed = 0,           \* runs the future owes the tasks before its next poll
    age = [p \in Places |-> 0], \* turns each place held work and was not served
    took = "none",      \* the place this turn took its runnable from
    polled = FALSE,     \* the future was polled this turn
    held = {},          \* the places that held work as the turn began
    homeOverInbox = FALSE; \* (record) a home was served while the inbox held work

define
    \* Whether a place holds work.
    Holds(p) == CASE p = "lifo" -> lifo > 0
                  [] p = "local" -> local > 0
                  [] p = "inbox" -> inbox > 0
                  [] p = "home" -> home > 0
                  [] p = "future" -> woken
    \* Every place that holds work is served within Bound turns.
    Served == \A p \in Places : age[p] <= Bound
    \* Witness: a home served while the inbox held work.
    NoHomeOverInbox == ~homeOverInbox
end define;

fair process loop = "loop"
begin
T_arrive:
    while TRUE do
        \* Other threads push into the inbox and the home, and wake the future, as they like.
        with i \in {inbox, 1}, h \in {home, 1}, w \in {woken, Future} do
            inbox := i;
            home := h;
            woken := w;
        end with;
T_poll:
        \* The places that hold work as the turn begins.
        held := {p \in Places : Holds(p)};
        \* The future is polled when it was woken and owes nothing; its poll may wake it
        \* again (a yield) and may queue a runnable (outside a task: at the local queue's
        \* back). A future that woke itself owes the tasks queued now one run each.
        if Future /\ woken /\ owed = 0 then
            with again \in BOOLEAN, push \in (IF local < Cap THEN {0, 1} ELSE {0}) do
                polled := TRUE;
                woken := again;
                owed := IF again THEN lifo + local + push ELSE 0;
                local := local + push;
            end with;
        end if;
T_interval:
        \* `next`: every I-th turn looks at the remote places first.
        \* MUTATION no_inbox_interval: no turn looks at them first.
        if until = 1 /\ Mutation # "no_inbox_interval" then
            until := I;
            if inbox > 0 then
                \* The inbox has work. Every other such turn (the proposed fix) a home's
                \* runnable runs first and the inbox's goes to the local queue's front.
                \* MUTATION home_after_empty: the inbox's always runs, as the code does today.
                with first = (Mutation # "home_after_empty" /\ ~homesFirst) do
                    homesFirst := first;
                    if first /\ home > 0 then
                        home := home - 1;
                        inbox := inbox - 1;
                        local := local + 1;
                        took := "home";
                    else
                        inbox := inbox - 1;
                        took := "inbox";
                    end if;
                end with;
            elsif home > 0 then
                \* The inbox is empty: the home's (`pop_inbox` falls back to it).
                home := home - 1;
                took := "home";
            end if;
        elsif Mutation # "no_inbox_interval" then
            until := until - 1;
        end if;
T_slot:
        if took = "none" then
            \* The slot runs again, fewer than L times in a row ...
            \* MUTATION no_lifo_cap: the slot runs however many times in a row.
            if lifo > 0 /\ (streak < L \/ Mutation = "no_lifo_cap") then
                lifo := 0;
                streak := IF streak < L THEN streak + 1 ELSE streak;
                took := "lifo";
            \* ... or, with nothing else queued, at once all the same ...
            elsif lifo > 0 /\ local = 0 then
                lifo := 0;
                streak := 0;
                took := "lifo";
            \* ... else it goes to the local queue's back, and the front one runs.
            elsif lifo > 0 then
                lifo := 0;
                streak := 0;
                took := "local";
            else
                \* An empty slot: the local queue, then the inbox, then the home.
                streak := 0;
                if local > 0 then
                    local := local - 1;
                    took := "local";
                elsif inbox > 0 then
                    inbox := inbox - 1;
                    took := "inbox";
                elsif home > 0 then
                    home := home - 1;
                    took := "home";
                end if;
            end if;
        end if;
T_run:
        \* The run of what was taken: one run less owed (none once nothing was left to
        \* run), and what it pushes.
        if took = "none" then
            owed := 0;
        else
            with push \in {"none"} \cup (IF local < Cap THEN {"wake", "yield"} ELSE {}) do
                if push = "wake" then
                    \* A task it woke: to the slot, the slot's runnable before it to the
                    \* local queue's back.
                    local := local + lifo;
                    lifo := 1;
                elsif push = "yield" then
                    \* Its own yield: to the local queue's back.
                    local := local + 1;
                end if;
                \* MUTATION owed_grows: each runnable a run pushes is owed a run too, so a
                \* pair of tasks that keep waking each other keeps the future from its poll.
                owed := (IF owed > 0 THEN owed - 1 ELSE 0)
                        + (IF Mutation = "owed_grows" /\ push # "none" /\ owed > 0 THEN 1 ELSE 0);
            end with;
        end if;
T_age:
        \* A place served this turn waited for nothing; one that held work and was not
        \* served waited one turn more; an empty one waits for nothing.
        age := [p \in Places |->
                    IF p = took \/ (p = "future" /\ polled) THEN 0
                    ELSE IF p \in held THEN age[p] + 1 ELSE 0];
        if took = "home" /\ "inbox" \in held then
            homeOverInbox := TRUE;
        end if;
        took := "none";
        polled := FALSE;
        held := {};
    end while;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

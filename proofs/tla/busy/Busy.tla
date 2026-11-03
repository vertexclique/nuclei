-------------------------------- MODULE Busy --------------------------------
(***************************************************************************)
(* How long a shard has been parked, as a reader on any thread sees it    *)
(* (src/rt/counters.rs: `Counters::park_began`, `Counters::park_ended`,   *)
(* `Counters::read`, `Counters::publish`, `Parked::encode`,              *)
(* `Parked::decode`; src/rt/shard/run.rs: the park around                 *)
(* `drive_or_report(None)`, whose drive publishes the counts once more).  *)
(*                                                                          *)
(* Who acts. The shard's thread runs, then parks, then wakes, over and    *)
(* over. A reader, any thread calling `nuclei::stats()`, reads the shard's *)
(* one published word and then its own clock, at any moment. The clock    *)
(* ticks between any two steps.                                           *)
(*                                                                          *)
(* The word. Not parked, it holds the time parked in all, `p`. Parked     *)
(* since `b`, it holds `b - p` with the parked flag set, and the parked   *)
(* time at `now` is `now - (b - p)`: the parks before plus this one so    *)
(* far. One word is one load, so a reader never sees half of a change.    *)
(* The shard also publishes its counts on its way into the kernel, inside *)
(* the park; that publish leaves a parked word as it is.                  *)
(*                                                                          *)
(* What must never happen. A reader's parked time below the true parked   *)
(* time at the moment it loaded the word (a sleeping shard read as busy,  *)
(* the bug of 0.5.4), or above it by more than the time between its load  *)
(* and its clock read (a busy shard read as asleep).                      *)
(***************************************************************************)
EXTENDS Naturals                                                           \* numbers and their operators

CONSTANTS                                                                  \* what each configuration sets:
    MaxT,       \* the last tick of the clock the run looks at
    Mutation    \* "none"; "no_store_begin": a park leaves the word as it was (0.5.4 stored it only before);
                \* "no_store_end": a wake leaves the word saying parked;
                \* "publish_overwrites": the publish inside a park stores the total over the parked word

VARIABLES                                                                  \* the state:
    t,          \* the clock, in ticks
    mode,       \* the shard: "run" or "park"
    p,          \* the time the shard spent parked in parks that ended
    b,          \* the tick the current park began (meaningful while parked)
    word,       \* the published word: [parked |-> flag, val |-> number]
    rs,         \* the reader: "idle", "loaded" (word read), "done" (clock read too)
    rw,         \* the word the reader loaded
    rload,      \* the tick of the reader's load
    rtruth,     \* the true parked time at the reader's load (a ghost: the code cannot see it)
    rnow,       \* the tick of the reader's clock read
    rdec        \* the parked time the reader decoded

vars == <<t, mode, p, b, word, rs, rw, rload, rtruth, rnow, rdec>>         \* every variable

\* The true parked time at tick `x`: the ended parks, plus the current one so far.
Truth(x) == p + (IF mode = "park" THEN x - b ELSE 0)                         \* what the reader should see
\* The word for "not parked, `q` parked in all".
Ran(q) == [parked |-> FALSE, val |-> q]                                     \* the flag clear
\* The word for "parked since `s`, `q` parked before": `s - q`, never below zero since no shard parks longer than it lived.
Parks(s, q) == [parked |-> TRUE, val |-> s - q]                            \* the flag set
\* What a reader decodes from word `w` at its clock `now`.
Decode(w, now) == IF w.parked THEN now - w.val ELSE w.val                   \* `Parked::decode`

Init ==                                                                    \* at the start:
    /\ t = 0                                                               \* the clock at zero
    /\ mode = "run"                                                        \* the shard running
    /\ p = 0                                                               \* never parked
    /\ b = 0                                                               \* no park begun
    /\ word = Ran(0)                                                       \* the word says so
    /\ rs = "idle"                                                         \* the reader not reading
    /\ rw = Ran(0)                                                         \* nothing loaded yet
    /\ rload = 0 /\ rtruth = 0 /\ rnow = 0 /\ rdec = 0                     \* nothing recorded yet

\* The clock ticks.
Tick ==                                                                    \* one tick passes
    /\ t < MaxT                                                            \* within the run's bound
    /\ t' = t + 1                                                          \* the clock moves on
    /\ UNCHANGED <<mode, p, b, word, rs, rw, rload, rtruth, rnow, rdec>>   \* nothing else moves

\* The shard parks now, and publishes the park in the word (`Counters::park_began`).
Park ==                                                                    \* the shard goes to sleep
    /\ mode = "run"                                                        \* it was running
    /\ mode' = "park"                                                      \* now it sleeps
    /\ b' = t                                                              \* since now
    \* MUTATION no_store_begin: the word stays as the last publish left it.
    /\ word' = IF Mutation = "no_store_begin" THEN word ELSE Parks(t, p)   \* parked since now
    /\ UNCHANGED <<t, p, rs, rw, rload, rtruth, rnow, rdec>>               \* the rest stays

\* The shard wakes now: the park's time joins the total, and the word says so (`Counters::park_ended`).
Wake ==                                                                    \* the shard wakes
    /\ mode = "park"                                                       \* it was asleep
    /\ mode' = "run"                                                       \* now it runs
    /\ p' = p + (t - b)                                                    \* the park's time counted
    \* MUTATION no_store_end: the word goes on saying parked.
    /\ word' = IF Mutation = "no_store_end" THEN word ELSE Ran(p + (t - b))  \* the new total
    /\ UNCHANGED <<t, b, rs, rw, rload, rtruth, rnow, rdec>>               \* the rest stays

\* The shard publishes its counts (`Counters::publish`): when running, and once more inside a park, on its
\* way into the kernel. A parked word stays as it is.
Publish ==                                                                 \* the counts published
    \* MUTATION publish_overwrites: the total goes over the parked word too.
    /\ word' = IF word.parked /\ Mutation # "publish_overwrites" THEN word ELSE Ran(p)  \* the total, unless parked
    /\ UNCHANGED <<t, mode, p, b, rs, rw, rload, rtruth, rnow, rdec>>      \* the rest stays

\* The reader loads the word; the truth at that tick is noted for the check.
Load ==                                                                    \* one load of the word
    /\ rs = "idle"                                                         \* between reads
    /\ rs' = "loaded"                                                      \* the word read
    /\ rw' = word                                                          \* what it holds now
    /\ rload' = t                                                          \* at this tick
    /\ rtruth' = Truth(t)                                                  \* the true parked time now
    /\ UNCHANGED <<t, mode, p, b, word, rnow, rdec>>                       \* the rest stays

\* The reader reads its clock and decodes the word with it.
Clock ==                                                                   \* the clock read
    /\ rs = "loaded"                                                       \* after its load
    /\ rs' = "done"                                                        \* the read complete
    /\ rnow' = t                                                           \* its clock now
    /\ rdec' = Decode(rw, t)                                               \* the parked time it reports
    /\ UNCHANGED <<t, mode, p, b, word, rw, rload, rtruth>>                \* the rest stays

\* The reader is ready to read again.
Again ==                                                                   \* the next read may start
    /\ rs = "done"                                                         \* one read ended
    /\ rs' = "idle"                                                        \* ready for another
    /\ UNCHANGED <<t, mode, p, b, word, rw, rload, rtruth, rnow, rdec>>    \* the rest stays

Next == Tick \/ Park \/ Wake \/ Publish \/ Load \/ Clock \/ Again            \* any of them, in any order
Spec == Init /\ [][Next]_vars                                              \* every behaviour of the steps

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* A read reports at least the parked time the shard had when the word was loaded: a sleeping shard is never read as busy.
NeverBelow == rs = "done" => rdec >= rtruth                                 \* not below the truth at the load
\* A read reports at most that, plus the ticks between its load and its clock read: a busy shard is never read as asleep.
NeverAboveGap == rs = "done" => rdec <= rtruth + (rnow - rload)             \* within the read's own gap
=============================================================================

------------------------------ MODULE LongPoll ------------------------------
(***************************************************************************)
(* The task a runner notes for each poll, and the monitor's report of a    *)
(* poll that runs long (`Builder::on_long_poll`).                          *)
(*                                                                          *)
(* The Rust it models: `compose`, `begin` and `end` (src/rt/notes.rs),     *)
(* wrapped around a runtime's poll hooks when `on_long_poll` is set;       *)
(* `Slot::note_task` and `Slot::task` (src/rt/handoff/slot.rs); the tick   *)
(* loop of `run` and `report_long` (src/rt/handoff/monitor.rs); `poll`     *)
(* and the bracket of `current` (src/rt/context.rs); the loop of           *)
(* `Shard::block_on` (src/rt/shard/run.rs), which polls its future first;  *)
(* and the order of a task's run (`run`, src/rt/task/raw.rs):             *)
(* `context::poll` goes out of the shard first, then the before-poll hook  *)
(* runs (where `begin` is), the future's poll, the after-poll hook (where  *)
(* `end` is), and only then `context::poll` comes back in.                 *)
(*                                                                          *)
(* In plain words: one thread, the runner, runs a worker's shard. It polls *)
(* tasks, and the futures of `block_on` calls, one at a time. Polls nest: *)
(* a task's code may call `block_on`, whose loop polls its own future      *)
(* first and then other tasks and its future again until the future is     *)
(* ready, and a future's code may call `block_on` too. Each poll leaves   *)
(* the shard with a new epoch: the slot's word says "outside, in the poll  *)
(* of epoch e" while the poll's code runs, and "inside" when the poll      *)
(* ends. A call into the runtime from a poll's code (a bracket, and a     *)
(* `block_on` is one) comes back inside and, when it returns, goes out    *)
(* again with the same word as before. A task's poll notes its task in    *)
(* the slot once it went out (`begin`: the slot's task becomes the poll's, *)
(* and the one it replaced goes on the thread's stack of notes), and puts  *)
(* the replaced one back at its end (`end`), before it comes back in. A   *)
(* block_on future's poll notes nothing, so the slot keeps naming the      *)
(* task whose poll encloses it. The monitor reads the word every tick: a  *)
(* read equal to the last one while outside extends a streak, and at       *)
(* `Streak` the poll is long. For a long word it did not report last, it  *)
(* remembers the word, reads the slot's task, and calls the hook with it, *)
(* unless it is 0 (nothing noted).                                         *)
(*                                                                          *)
(* What must never happen: the hook called with a task whose poll already *)
(* ended (ReportedIsOnStack); the slot read for a report naming anything  *)
(* but the innermost task whose begin ran and whose end has not            *)
(* (NamesInnermostNoted); the hook called twice for one poll (OncePerPoll, *)
(* which holds only while no poll nested in it is reported meanwhile: see  *)
(* W_twice_after_nesting.cfg).                                             *)
(*                                                                          *)
(* What is abstract. A task's id is its poll's epoch: every poll is a task *)
(* of its own, so a note left from an ended poll is caught even where the *)
(* same task polled again would make it look right. The worker loop polls *)
(* only tasks (its own future notes nothing and returns at once), and the *)
(* user's own poll hooks, which run outside the notes, are left out. The  *)
(* take attempt that comes between the read of the task and the call is   *)
(* left out: the hook gets the task read whatever the take does, and a    *)
(* take that succeeds only ends the watch of this slot (the next runner   *)
(* gets a fresh slot and the watch forgets the word it reported, the same *)
(* story from the start), so a model where every take fails sees every    *)
(* read this slot can see. After the task the monitor reads the word once  *)
(* more, and calls the hook only if it is still the long one, so a poll   *)
(* that ended meanwhile is not told under the task that came after it. A *)
(* step is atomic and seen at once: the read of the word is an acquire     *)
(* that pairs with the release of `go_out`, so the read of the task after  *)
(* it sees no value older than the one the slot had when that word was     *)
(* written; the task's store is a release and its read an acquire         *)
(* (`Slot::note_task`, `Slot::task`), so a later poll's task is not seen   *)
(* before the word that ended the earlier poll, and the second read of the *)
(* word is not made before the task's. That order is argued here, not     *)
(* checked: these interleavings are sequentially consistent. The monitor's *)
(* sleep while every worker parks resets its streaks; a parked runner's   *)
(* word is inside, and a read of it resets the streak the same way.       *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets                          \* numbers, the stack, set sizes

CONSTANTS
    MaxPolls,  \* polls the runner begins in all, each with a new epoch
    MaxDepth,  \* polls on the runner's stack at once at most (1: no poll calls block_on)
    Streak,    \* equal reads past the first that make a poll long (`STREAK`)
    Brackets,  \* whether a poll's code may call into the runtime other than through block_on
    Mode       \* "ok", or the name of the one rule this run breaks (see the configurations)

ASSUME MaxPolls \in Nat /\ MaxDepth \in Nat /\ MaxDepth > 0 /\ Streak \in Nat /\ Streak > 0  \* sizes that make sense
ASSUME Brackets \in BOOLEAN                                      \* calls in, or not
ASSUME Mode \in {"ok", "end_keeps", "no_stack", "one_cell", "no_memory", "report_zero",
                 "no_reread"}                                    \* a known rule

Out(e) == 2 * e + 1             \* the word of the poll of epoch e while outside (`slot::outside`)
In(e) == 2 * e                  \* the word of the poll of epoch e while inside (`slot::inside`)
IsOut(w) == w % 2 = 1           \* whether a word says outside (`slot::is_outside`)
EpochOf(w) == w \div 2          \* the epoch a word names
NoTask == 0                     \* the slot's task when nothing is noted (a task id is never 0)
Max(S) == CHOOSE m \in S : \A n \in S : n <= m   \* the largest number of a set

VARIABLES
    stack,     \* the runner's polls, outermost first: [kind, ep, ph, nest] each
    epochs,    \* the epochs begun so far: the last poll's (`Here::epochs`)
    word,      \* the slot's word (`Slot::word`), written by the runner only
    task,      \* the slot's task (`Slot::task`), written by `begin` and `end` only
    notes,     \* the thread's stack of notes (`POLLS`): the task each begin replaced
    last,      \* the monitor's last read of the word (`Watch::last`), an inside one kept as In(0)
    streak,    \* equal reads past the first while outside (`Watch::streak`)
    reported,  \* the last long word the monitor read the task for (`Watch::reported`)
    mpc,       \* the monitor's next step: "read" the word, read the "task", or "check" the word again
    det,       \* the long word whose task the monitor reads next (0 otherwise)
    got,       \* the task the monitor read for det, before its second look at the word (0 otherwise)
    hooked     \* the words, of polls still on the stack, the hook was called for

vars == <<stack, epochs, word, task, notes, last, streak, reported, mpc, det, got, hooked>>  \* every variable
monitorVars == <<last, streak, reported, mpc, det, got, hooked>>  \* what the monitor holds

(***************************************************************************)
(* A poll on the stack is a record:                                         *)
(*   kind  "task" (it notes its task) or "future" (a block_on's, it notes   *)
(*         nothing);                                                        *)
(*   ep    its epoch, which is also its task's id;                          *)
(*   ph    for a task's poll: "out" (gone out, begin not run yet), "noted"  *)
(*         (begin ran), "unnoted" (end ran); a future's stays "out";       *)
(*   nest  "body" (its code runs, outside), "call" (in a call into the     *)
(*         runtime, inside), "loop0" (in a block_on's loop that has not    *)
(*         polled its future yet), "loop" (in that loop, later), "ready"   *)
(*         (the loop's future returned Ready: the block_on returns next).  *)
(***************************************************************************)

Depth == Len(stack)                                              \* polls on the stack
Top == stack[Depth]                                              \* the innermost poll (when Depth > 0)
Running(f) == f.kind = "future" \/ f.ph = "noted"                \* the poll's own code may run
TaskAt(id) == {i \in 1..Depth : stack[i].kind = "task" /\ stack[i].ep = id}  \* where task id's poll is
OnStack(id) == TaskAt(id) # {}                                   \* task id's poll is on the stack
NotedAt == {i \in 1..Depth : stack[i].kind = "task" /\ stack[i].ph = "noted"} \* polls whose begin ran, end not
Noted == IF NotedAt = {} THEN NoTask ELSE stack[Max(NotedAt)].ep \* the innermost of them, or 0
Live == {Out(stack[i].ep) : i \in 1..Depth}                      \* outside words that can still come back
WillCall(id) == id # NoTask \/ Mode = "report_zero"              \* `report_long` calls the hook for id
\* MUTATION report_zero: the hook is called with 0 as well.

\* The innermost poll becomes f; the rest of the stack stays as it was.
SetTop(f) == stack' = [stack EXCEPT ![Depth] = f]

Init ==
    /\ stack = <<>>                                              \* the runner is in its worker's loop
    /\ epochs = 0                                                \* no poll begun
    /\ word = In(0)                                              \* a new slot's word (`Slot::new`)
    /\ task = NoTask                                             \* nothing noted
    /\ notes = <<>>                                              \* no note on the thread's stack
    /\ last = In(0)                                              \* the watch's first `last`
    /\ streak = 0                                                \* no streak
    /\ reported = In(0)                                          \* nothing reported: never a long word
    /\ mpc = "read"                                              \* the monitor reads the word first
    /\ det = 0                                                   \* no long word pending
    /\ got = 0                                                   \* no task read yet
    /\ hooked = {}                                               \* the hook was not called

(***************************************************************************)
(* The runner.                                                              *)
(***************************************************************************)

\* A loop starts a poll of kind k (`context::poll`: a new epoch, `go_out`): the worker's loop at
\* the top of the stack, or a block_on's loop, whose first poll is its own future's.
Start(k) ==
    /\ epochs < MaxPolls                                         \* a poll left to begin
    /\ Depth < MaxDepth                                          \* and room on the stack
    /\ IF Depth = 0                                              \* the worker's loop
       THEN k = "task"                                           \* polls tasks
       ELSE \/ Top.nest = "loop0" /\ k = "future"                \* a block_on's loop, its future first
            \/ Top.nest = "loop"                                 \* then a task's poll or the future's
    /\ epochs' = epochs + 1                                      \* the poll's epoch
    /\ word' = Out(epochs + 1)                                   \* outside, in it (a release store)
    /\ stack' = Append(IF Depth > 0 /\ Top.nest = "loop0"        \* a loop polling its future the
                       THEN [stack EXCEPT ![Depth].nest = "loop"]  \* first time is past its first poll
                       ELSE stack,                               \* any other loop stays as it was
                       [kind |-> k, ep |-> epochs + 1, ph |-> "out", nest |-> "body"])  \* the new poll
    /\ UNCHANGED <<task, notes>>                                 \* nothing noted yet
    /\ UNCHANGED monitorVars                                     \* the monitor is not touched

\* The before-poll hook of a task's poll: `begin` swaps the poll's task into the slot
\* (`Slot::note_task`) and pushes the task it replaced on the thread's stack of notes.
\* MUTATION no_stack: nothing is kept, and `end` then puts 0 back (as a single slot with no
\* stack would, and as an `end` that writes 0 does).
\* MUTATION one_cell: the replaced task is kept in one cell, not a stack: a nested begin
\* overwrites it.
Begin ==
    /\ Depth > 0                                                 \* a poll is on the stack
    /\ Top.kind = "task" /\ Top.ph = "out"                       \* a task's, its begin not run yet
    /\ task' = Top.ep                                            \* the slot names the poll's task
    /\ notes' = CASE Mode = "no_stack" -> notes                  \* the planted bug: nothing kept
                  [] Mode = "one_cell" -> <<task>>               \* the planted bug: one cell, overwritten
                  [] OTHER -> Append(notes, task)                \* the replaced task, pushed
    /\ SetTop([Top EXCEPT !.ph = "noted"])                       \* the poll's task is noted
    /\ UNCHANGED <<epochs, word>>                                \* the word stays outside
    /\ UNCHANGED monitorVars                                     \* the monitor is not touched

\* The after-poll hook of a task's poll, once its future's poll returned: `end` pops the poll's
\* note and puts the task it replaced back in the slot, before the poll comes back in.
\* MUTATION end_keeps: the slot keeps the ended poll's task.
End ==
    /\ Depth > 0                                                 \* a poll is on the stack
    /\ Top.kind = "task" /\ Top.ph = "noted"                     \* a task's, its begin run
    /\ Top.nest = "body"                                         \* its code returned
    /\ task' = CASE Mode = "end_keeps" -> task                   \* the planted bug: left as it is
                 [] Mode = "no_stack" -> NoTask                  \* nothing was kept: 0
                 [] Mode = "one_cell" -> notes[1]                \* the one cell, as the last begin left it
                 [] OTHER -> notes[Len(notes)]                   \* the task this poll's begin replaced
    /\ notes' = IF Mode \in {"no_stack", "one_cell"}             \* no stack to pop
                THEN notes                                       \* in those two
                ELSE SubSeq(notes, 1, Len(notes) - 1)            \* the poll's note popped
    /\ SetTop([Top EXCEPT !.ph = "unnoted"])                     \* the poll's task is noted no more
    /\ UNCHANGED <<epochs, word>>                                \* the word is still outside
    /\ UNCHANGED monitorVars                                     \* the monitor is not touched

\* The poll's code calls into the runtime (the bracket of `current`): inside, in the poll's epoch.
CallIn ==
    /\ Brackets                                                  \* this run lets a poll's code call in
    /\ Depth > 0 /\ Top.nest = "body" /\ Running(Top)            \* the poll's own code runs
    /\ word' = In(Top.ep)                                        \* inside, in the poll's epoch
    /\ SetTop([Top EXCEPT !.nest = "call"])                      \* in the call
    /\ UNCHANGED <<epochs, task, notes>>                         \* nothing noted
    /\ UNCHANGED monitorVars                                     \* the monitor is not touched

\* The call returns (`Bracket::drop`): outside again, with the poll's word as before the call.
CallOut ==
    /\ Depth > 0 /\ Top.nest = "call"                            \* in a call into the runtime
    /\ word' = Out(Top.ep)                                       \* outside, the same word again
    /\ SetTop([Top EXCEPT !.nest = "body"])                      \* back in the poll's code
    /\ UNCHANGED <<epochs, task, notes>>                         \* nothing noted
    /\ UNCHANGED monitorVars                                     \* the monitor is not touched

\* The poll's code calls block_on (`block_on_here`, through a bracket of `current` that lasts the
\* whole call): inside, in the poll's epoch, the runner runs the block_on's loop.
BlockOn ==
    /\ Depth > 0 /\ Top.nest = "body" /\ Running(Top)            \* the poll's own code runs
    /\ Depth < MaxDepth                                          \* room for the loop's polls
    /\ epochs < MaxPolls                                         \* and a poll left for its future
    /\ word' = In(Top.ep)                                        \* inside, in the poll's epoch
    /\ SetTop([Top EXCEPT !.nest = "loop0"])                     \* in the loop, its future not polled
    /\ UNCHANGED <<epochs, task, notes>>                         \* nothing noted
    /\ UNCHANGED monitorVars                                     \* the monitor is not touched

\* A poll comes back in at its end (`Out::back` in `context::poll`): a task's after its end, a
\* future's when its poll returns; a block_on future's that returned Ready ends its loop. The
\* poll's outside word is never written again (epochs only grow), so the monitor's memory of
\* it means the same as none: the model forgets it, which merges states that behave alike.
ComeBack ==
    /\ Depth > 0 /\ Top.nest = "body"                            \* the poll's code returned
    /\ (Top.kind = "future" \/ Top.ph = "unnoted")               \* a task's only after its end
    /\ word' = In(Top.ep)                                        \* inside, in the poll's epoch
    /\ \E ready \in BOOLEAN :                                    \* the future's poll was Ready or not
         stack' = IF Depth > 1 /\ Top.kind = "future" /\ ready   \* a block_on's future, Ready:
                  THEN [SubSeq(stack, 1, Depth - 1) EXCEPT ![Depth - 1].nest = "ready"]  \* its loop ends
                  ELSE SubSeq(stack, 1, Depth - 1)               \* else the poll just leaves the stack
    /\ last' = IF last = Out(Top.ep) THEN In(0) ELSE last        \* forgotten
    /\ streak' = IF last = Out(Top.ep) THEN 0 ELSE streak        \* the next read would reset it anyway
    /\ reported' = IF reported = Out(Top.ep) THEN In(0) ELSE reported  \* forgotten
    /\ hooked' = hooked \ {Out(Top.ep)}                          \* forgotten
    /\ UNCHANGED <<epochs, task, notes, mpc, det, got>>          \* nothing noted; a pending read stays

\* The block_on returns: the bracket around it drops (`Bracket::drop`), and the runner goes out
\* again with the poll's own word, the one it had before the call.
Return ==
    /\ Depth > 0 /\ Top.nest = "ready"                           \* the loop's future was Ready
    /\ word' = Out(Top.ep)                                       \* outside, the same word again
    /\ SetTop([Top EXCEPT !.nest = "body"])                      \* back in the poll's code
    /\ UNCHANGED <<epochs, task, notes>>                         \* nothing noted
    /\ UNCHANGED monitorVars                                     \* the monitor is not touched

(***************************************************************************)
(* The monitor.                                                             *)
(***************************************************************************)

\* A tick: the monitor reads the word (`Slot::word`, an acquire). Equal to the last read while
\* outside, the streak grows; at `Streak` the poll is long and the streak starts again. A long
\* word other than the last one reported (`watch.reported != word`) becomes the one reported,
\* and its task is read next.
\* MUTATION no_memory: the monitor does not remember the word it reported.
Read ==
    /\ mpc = "read"                                              \* the monitor's turn to read the word
    /\ LET s == IF IsOut(word) /\ word = last THEN streak + 1 ELSE 0  \* the streak with this read
           long == s >= Streak                                   \* the poll is long
           fresh == long /\ (word # reported \/ Mode = "no_memory")  \* and not the last one reported
       IN /\ last' = IF IsOut(word) THEN word ELSE In(0)         \* an inside word never extends a streak
          /\ streak' = IF long THEN 0 ELSE s                     \* it starts again once long
          /\ reported' = IF fresh THEN word ELSE reported        \* remembered as reported
          /\ mpc' = IF fresh THEN "task" ELSE "read"             \* its task is read next
          /\ det' = IF fresh THEN word ELSE 0                    \* for this word
    /\ UNCHANGED <<stack, epochs, word, task, notes, got, hooked>>  \* the runner is not touched

\* The monitor reads the slot's task (`Slot::task`); its look at the word again comes next.
ReadTask ==
    /\ mpc = "task"                                              \* a long word not reported last
    /\ got' = task                                               \* the task, as the slot names it now
    /\ mpc' = "check"                                            \* the second look at the word is next
    /\ UNCHANGED <<stack, epochs, word, task, notes, last, streak, reported, det, hooked>>  \* the rest

\* The monitor reads the word again: still the long one, it calls the hook with the task it read,
\* unless that is 0 (`report_long`); another word, and the long poll ended, and nothing is called.
\* The take attempt between the read and the call is left out (see the header).
\* MUTATION no_reread: the word is not read again, and the hook is called whatever it is now.
Check ==
    /\ mpc = "check"                                             \* the task was read
    /\ hooked' = IF WillCall(got) /\ (word = det \/ Mode = "no_reread")  \* the hook is called:
                 THEN (hooked \cup {det}) \cap Live              \* for det, if its poll is still on
                 ELSE hooked                                     \* nothing called
    /\ mpc' = "read"                                             \* the next tick reads the word
    /\ det' = 0                                                  \* nothing pending
    /\ got' = 0                                                  \* nothing read
    /\ UNCHANGED <<stack, epochs, word, task, notes, last, streak, reported>>  \* the rest as it was

Next ==
    \/ Start("task") \/ Start("future")                          \* a loop starts a poll
    \/ Begin \/ End                                              \* a task's poll notes and un-notes
    \/ CallIn \/ CallOut                                         \* a call into the runtime
    \/ BlockOn \/ Return                                         \* a block_on called and returning
    \/ ComeBack                                                  \* a poll ends
    \/ Read \/ ReadTask \/ Check                                 \* the monitor's tick and report

Spec == Init /\ [][Next]_vars                                    \* every behaviour; only safety is checked

(***************************************************************************)
(* The properties. The monitor is about to read the slot's task in every   *)
(* state with mpc = "task"; checking those states checks every read, as    *)
(* the read sees the state it is taken in.                                 *)
(***************************************************************************)

\* The monitor is about to read the slot's task, and the hook will be called with it.
Calling == mpc = "task" /\ WillCall(task)

\* The hook is only ever called with a task whose poll is on the runner's stack when the
\* monitor reads it, never with one whose poll already ended.
ReportedIsOnStack == Calling => OnStack(task)

\* The task the monitor reads for a report is the innermost one whose begin ran and whose end
\* has not: the poll the runner is in, or, for a block_on future, the task whose poll encloses
\* it; 0 only when no task's poll is noted.
NamesInnermostNoted == mpc = "task" => task = Noted

\* The monitor is about to call the hook with the task it read.
Calls == mpc = "check" /\ WillCall(got) /\ (word = det \/ Mode = "no_reread")

\* The hook is called at most once for a poll: never again for a word it was called for.
OncePerPoll == Calls => det \notin hooked

\* The hook never names the task of a poll that began after the long one, in a run where no
\* poll nests (with nesting, a poll may begin and end inside the long one between the monitor's
\* reads: W_newer_nested.cfg).
NoNewerName == Calls => got <= EpochOf(det)

\* The lemma under the first two: the slot names the innermost noted task at every moment.
SlotTracksStack == task = Noted

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* The hook names a task whose poll is nested in another poll (a task a block_on's loop polled).
NoNestedReport == Calling => \A i \in TaskAt(task) : i = 1

\* The hook names the task whose poll encloses the innermost poll, a block_on future's.
NoEnclosingReport == Calling => ~(Depth > 0 /\ Top.kind = "future" /\ \E i \in TaskAt(task) : i < Depth)

=============================================================================

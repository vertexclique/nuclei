------------------------------ MODULE ListOwner -----------------------------
(***************************************************************************)
(* Who may edit a shard's list of tasks when the shard is taken from its   *)
(* runner while that runner is in a poll (src/rt/context.rs:               *)
(* `run_task_watched`, `current_held`; src/rt/sched.rs: `Sched::pending`,  *)
(* `Sched::drop_future`, `Sched::list`, `Sched::unlist`;                   *)
(* src/rt/shard/list_op.rs: `ListOp::apply`, `ListOp::late`, posted       *)
(* through the shard's io_ops mailbox (src/rt/shard.rs: `post_io`, the    *)
(* applies of io_ops, `IoOp::gone` as the shard goes);                    *)
(* src/rt/task/list.rs: `TaskList::push`, `Links::unlink`,                 *)
(* `TaskList::cancel_all`; src/rt/shard/going.rs: `Shard::go`).            *)
(*                                                                          *)
(* Who acts. T1, the shard's runner, in a poll of task A that may block.   *)
(* The monitor, which may take the shard while T1 is in that poll and give *)
(* it to T2. T2, the shard's runner from then on, which keeps polling the  *)
(* shard's other task B (so it lists and unlists B), and may poll A once   *)
(* more after T1 is done with it (A woken again). The runtime's drop,     *)
(* which walks the shard's list on the owner's thread and cancels each    *)
(* task on it, and the shard's last holder, which handles what the        *)
(* shard's mailbox still gets as the shard goes.                           *)
(*                                                                          *)
(* The list. Intrusive: a task's links live in the task's own memory, no  *)
(* atomics, so only one thread may edit it at a time, and that thread is   *)
(* the list's owner: T1 until the take, T2 from it. A poll's result asks  *)
(* for an edit: a pending of an unlisted A lists it, a ready of a listed  *)
(* or posted A unlists it; the future is dropped where the ready is.       *)
(*                                                                          *)
(* The guard. After the poll, T1 comes back into the shard (a bracket). If *)
(* it is still the owner it edits the list itself. If the shard was taken *)
(* it edits nothing: it posts the edit to the shard's mailbox with a      *)
(* reference to A, so A's memory stays until the owner applies it. The    *)
(* owner applies a posted list only to a task that is still live and not  *)
(* listed (the owner may have listed it itself meanwhile, or the task may *)
(* have ended), and a posted unlist only to a listed task. After the walk *)
(* a posted list does not link: it ends the task at once.                 *)
(*                                                                          *)
(* X, any thread off the owner that drops A's future (a cancel of an idle *)
(* A, a runnable dropped elsewhere), never edits and never decides from   *)
(* A's links: it posts an unlist with a reference, and the owner unlinks A *)
(* if A is listed when it applies it.                                      *)
(*                                                                          *)
(* What must never happen. A thread that is not the list's owner edits    *)
(* it (two threads writing the same links: a null link, a lost task, a    *)
(* crash); an edit touches A's links after A's memory went; A's memory     *)
(* goes while A is still linked; A is linked twice; A is on the list after *)
(* its future went (a walk would end a task that has no future); the       *)
(* runtime's drop ends with A still live and on no list, so nothing ends  *)
(* it.                                                                     *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets                                              \* numbers and finite sets

CONSTANTS
    AListed,    \* A was on the list before this poll (an earlier poll of A returned pending)
    Mutation    \* "none"; "edit_anyway": T1 edits the list though the shard was taken (the code of 0.5.4);
                \* "drop_edit": T1 posts nothing when the shard was taken; "no_ref": the posted edit holds no
                \* reference to A; "no_skip_listed": a posted list links A though the owner listed it already;
                \* "no_skip_ended": a posted list links A though A's future went meanwhile; "foreign_reads": a thread
                \* that drops A's future off the owner decides from A's links (a look the owner's edit can overtake)

VARIABLES
    owner,      \* who owns the shard's list: "T1" or "T2"
    t1,         \* T1: "poll" (in A's poll), "back" (the poll returned, its edit not done), "done"
    res,        \* what T1's poll of A returned: "none" yet, "pending" or "ready"
    links,      \* how many times A is linked into the shard's list: 0, 1 (or 2: a double link)
    listB,      \* B is on the shard's list
    mail,       \* the edits posted to the shard's mailbox: a subset of {"listA", "unlistA"} (T1's) and "xUnlist" (X's)
    ended,      \* A's future went (A ended, or was cancelled)
    freed,      \* A's memory went
    repolled,   \* the owner polled A once more after T1 was done with it
    xLook,      \* X, a thread off the owner dropping A's future (a cancel): "none", or what it saw of A's links ("on", "off")
    phase,      \* the runtime: "run", "walked" (the drop walked the list), "gone" (the shard went)
    badEdit,    \* (record) a thread that is not the owner edited the list
    uaf         \* (record) an edit touched A's links after A's memory went

vars == <<owner, t1, res, links, listB, mail, ended, freed, repolled, xLook, phase, badEdit, uaf>>  \* every variable

Listed == links > 0                                                        \* A is on the list

Init ==                                                                    \* at the start:
    /\ owner = "T1"                                                        \* T1 runs the shard
    /\ t1 = "poll"                                                         \* in A's poll
    /\ res = "none"                                                        \* the poll has not returned
    /\ links = IF AListed THEN 1 ELSE 0                                    \* A listed or not, as the configuration says
    /\ listB = FALSE                                                       \* B not listed yet
    /\ mail = {}                                                           \* nothing posted
    /\ ended = FALSE                                                       \* A's future is there
    /\ freed = FALSE                                                       \* A's memory is there
    /\ repolled = FALSE                                                    \* the owner has not polled A again
    /\ xLook = "none"                                                      \* X has not started
    /\ phase = "run"                                                       \* the runtime runs
    /\ badEdit = FALSE                                                     \* nothing went wrong
    /\ uaf = FALSE                                                         \* nothing went wrong

\* The posted edits of `m` that hold a reference to A: each holds its own (MUTATION no_ref: T1's hold none).
Holding(m) == {e \in m : e = "xUnlist" \/ Mutation # "no_ref"}             \* the posted references
\* A's memory goes once its future went and no posted edit holds it (a list holds no reference).
Free(e, m) == freed \/ (e /\ Holding(m) = {})                               \* the memory's fate after a step

\* The monitor takes the shard while T1 is in its poll and gives it to T2.
Take ==                                                                    \* the take
    /\ t1 = "poll" /\ owner = "T1" /\ phase = "run"                       \* T1 blocked in the poll, before the drop
    /\ owner' = "T2"                                                       \* T2 owns the list from now on
    /\ UNCHANGED <<t1, res, links, listB, mail, ended, freed, repolled, xLook, phase, badEdit, uaf>>  \* the rest stays

\* The owner, T2, lists B (B's poll returned pending): an edit of the list by its owner.
OwnerListsB ==                                                             \* B listed
    /\ owner = "T2" /\ phase = "run" /\ ~listB                            \* the owner, B not listed
    /\ listB' = TRUE                                                       \* B on the list
    /\ UNCHANGED <<owner, t1, res, links, mail, ended, freed, repolled, xLook, phase, badEdit, uaf>>  \* the rest stays

\* The owner, T2, unlists B (B ended).
OwnerUnlistsB ==                                                           \* B unlisted
    /\ owner = "T2" /\ phase = "run" /\ listB                             \* the owner, B listed
    /\ listB' = FALSE                                                      \* B off the list
    /\ UNCHANGED <<owner, t1, res, links, mail, ended, freed, repolled, xLook, phase, badEdit, uaf>>  \* the rest stays

\* T1's poll of A returns, pending or ready. A pending run that finds its reference A's last one, with no handle and
\* no waker left (`drop_ref`, `drop_unawaited`), drops A's future on T1 just as a ready does: it is "ready" here.
Return(r) ==                                                               \* the poll's result
    /\ t1 = "poll"                                                         \* still in it
    /\ t1' = "back" /\ res' = r                                            \* back, with its result
    /\ UNCHANGED <<owner, links, listB, mail, ended, freed, repolled, xLook, phase, badEdit, uaf>>  \* the rest stays

\* The edit T1's result asks for: list an unlisted A on a pending, unlist a listed A on a ready.
Wanted == IF res = "pending" /\ ~Listed THEN {"listA"}                    \* a first pending lists A
          ELSE IF res = "ready" /\ Listed THEN {"unlistA"}                \* a ready unlists a listed A
          ELSE {}                                                          \* nothing to edit

\* After the poll, T1 comes back into the shard and does A's edit: itself when it still owns the list, else it posts it.
T1Edit ==                                                                  \* T1's post-poll bookkeeping
    /\ t1 = "back"                                                         \* the poll returned
    /\ t1' = "done"                                                        \* bookkeeping done
    /\ LET mine == owner = "T1" \/ Mutation = "edit_anyway"                \* MUTATION edit_anyway: T1 edits as if it still owned it
           post == owner = "T2" /\ Mutation # "edit_anyway" /\ Mutation # "drop_edit" \* MUTATION drop_edit: nothing posted
           e == ended \/ res = "ready"                                     \* a ready drops A's future here, on T1
           m == IF post THEN mail \cup Wanted ELSE mail                    \* the mailbox after T1's post
       IN /\ links' = IF mine /\ "listA" \in Wanted THEN links + 1
                      ELSE IF mine /\ "unlistA" \in Wanted THEN links - 1
                      ELSE links                                           \* T1's own edit, if it makes one
          /\ badEdit' = (badEdit \/ (mine /\ owner # "T1" /\ Wanted # {}))  \* recorded: an edit by a thread that is not the owner
          /\ uaf' = (uaf \/ (mine /\ freed /\ Wanted # {}))                \* recorded: an edit of freed links
          /\ mail' = m                                                     \* posted to the owner's mailbox
          /\ ended' = e                                                    \* A's future gone on a ready
          /\ freed' = Free(e, m)                                           \* A's memory goes once nothing holds it
    /\ UNCHANGED <<owner, res, listB, repolled, xLook, phase>>                    \* the rest stays

\* The owner polls A once more after T1 is done with it (A was woken again): a pending lists an unlisted A
\* itself, a ready ends A and unlists it if it is listed; either way the owner's own edit.
OwnerPollsA(r) ==                                                          \* the owner's poll of A
    /\ owner = "T2" /\ t1 = "done" /\ phase = "run"                       \* after T1's bookkeeping, before the drop
    /\ ~ended /\ ~repolled                                                 \* A live, once
    /\ repolled' = TRUE                                                    \* polled again
    /\ IF r = "pending"
         THEN /\ links' = IF Listed THEN links ELSE links + 1             \* listed by its owner on a first pending
              /\ ended' = ended /\ freed' = freed                          \* A goes on
         ELSE /\ links' = IF Listed THEN links - 1 ELSE links             \* a ready unlists a listed A (`drop_future`)
              /\ ended' = TRUE                                             \* A's future dropped here
              /\ freed' = Free(TRUE, mail)                                 \* A's memory goes once no posted reference holds it
    /\ uaf' = (uaf \/ freed)                                               \* recorded: a poll of freed memory
    /\ UNCHANGED <<owner, t1, res, listB, mail, xLook, phase, badEdit>>  \* the rest stays

\* The owner applies one posted edit at a turn of its loop. Before the walk: a posted list links A only while A
\* is live and unlisted, a posted unlist unlinks a listed A. After the walk: a posted list ends A, links nothing.
Apply ==                                                                   \* one posted edit applied
    /\ mail # {} /\ owner = "T2"                                           \* something posted, T2 owns the list
    /\ \E e \in mail :                                                     \* any posted edit
        /\ mail' = mail \ {e}                                              \* taken from the mailbox
        /\ uaf' = (uaf \/ freed)                                           \* recorded: an edit of freed links
        /\ LET skipListed == Mutation # "no_skip_listed"                   \* MUTATION no_skip_listed: links a listed A again
               skipEnded == Mutation # "no_skip_ended"                     \* MUTATION no_skip_ended: links an ended A
               link == phase = "run" /\ e = "listA"
                       /\ ~(skipListed /\ Listed) /\ ~(skipEnded /\ ended) \* a posted list that links
               unlink == e \in {"unlistA", "xUnlist"} /\ Listed            \* a posted unlist of a listed A
               late == phase # "run" /\ e = "listA" /\ ~ended              \* after the walk: the list ends A at once
           IN /\ links' = IF link THEN links + 1 ELSE IF unlink THEN links - 1 ELSE links  \* the edit, if any
              /\ ended' = (ended \/ late)                                  \* A cancelled by a late list
              /\ freed' = Free(ended \/ late, mail \ {e})                 \* the applied edit's reference goes with it
    /\ UNCHANGED <<owner, t1, res, listB, repolled, xLook, phase, badEdit>>       \* the rest stays

\* X drops A's future off the owner (a cancel of an idle A, a runnable dropped on another thread), in two steps so
\* the owner's edits can come between them. First X looks at A's links (only the mutation decides from that look).
XLook ==                                                                   \* X's look
    /\ xLook = "none" /\ ~ended /\ t1 = "done" /\ phase = "run"            \* A idle, after T1, before the drop
    /\ xLook' = IF Listed THEN "on" ELSE "off"                              \* what X saw
    /\ UNCHANGED <<owner, t1, res, links, listB, mail, ended, freed, repolled, phase, badEdit, uaf>>  \* the rest stays

\* Then X drops the future. The guard: X never edits and never decides from its look; it posts the unlist with a
\* reference, and the owner unlinks A if A is listed when it applies it. MUTATION foreign_reads: X posts only if it saw A on.
XDrop ==                                                                   \* X drops A's future
    /\ xLook # "none" /\ ~ended                                            \* after its look, A still live
    /\ xLook' = "none"                                                     \* done
    /\ ended' = TRUE                                                       \* A's future gone
    /\ LET post == Mutation # "foreign_reads" \/ xLook = "on"               \* whether X posts the unlist
           m == IF post THEN mail \cup {"xUnlist"} ELSE mail                 \* the mailbox after X's post
       IN /\ mail' = m                                                      \* posted to the owner
          /\ freed' = Free(TRUE, m)                                         \* A's memory goes once nothing holds it
    /\ UNCHANGED <<owner, t1, res, links, listB, repolled, phase, badEdit, uaf>>  \* the rest stays

\* The runtime's drop walks the shard's list on the owner's thread, cancelling each task on it. It walks once T1's
\* bookkeeping for A is done: a drop while T1 is still in A's poll is Teardown.tla's (T1 detached, waited for to a deadline).
Walk ==                                                                    \* the drop's walk
    /\ phase = "run" /\ t1 = "done"                                        \* once, after A's bookkeeping
    /\ phase' = "walked"                                                   \* walked
    /\ ended' = (ended \/ Listed)                                          \* A cancelled if it is on the list
    /\ links' = 0 /\ listB' = FALSE                                        \* every task off the list
    /\ freed' = Free(ended \/ Listed, mail)                                \* A's memory goes once nothing holds it
    /\ UNCHANGED <<owner, t1, res, mail, repolled, xLook, badEdit, uaf>>  \* the rest stays

\* The shard goes, once T1's bookkeeping is done and the mailbox is empty.
Go ==                                                                      \* the shard's end
    /\ phase = "walked" /\ t1 = "done" /\ mail = {}                        \* walked, nothing left to apply
    /\ phase' = "gone"                                                     \* gone
    /\ UNCHANGED <<owner, t1, res, links, listB, mail, ended, freed, repolled, xLook, badEdit, uaf>>  \* the rest stays

Next ==                                                                    \* any of them, in any order
    \/ Take \/ OwnerListsB \/ OwnerUnlistsB
    \/ Return("pending") \/ Return("ready")
    \/ T1Edit \/ OwnerPollsA("pending") \/ OwnerPollsA("ready")
    \/ Apply \/ XLook \/ XDrop \/ Walk \/ Go

Spec == Init /\ [][Next]_vars                                              \* every behaviour of the steps

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* Only the list's owner ever edits it.
OwnerOnly == ~badEdit
\* No edit touches A's links once A's memory went.
NoUseAfterFree == ~uaf
\* A's memory never goes while A is still on the list (a dangling node the owner's next walk would follow).
NoDangling == ~(freed /\ Listed)
\* A is never linked twice.
NoDoubleLink == links <= 1
\* A is on the list after its future went only while its posted unlist waits for the owner (the posted reference
\* keeps its memory, and the walk only unlinks it): never a dead task linked for good.
NoDeadListed == (ended /\ Listed) => (mail \cap {"unlistA", "xUnlist"}) # {}
\* When the shard went, A is not left live: it ended, by its own ready or by a cancel.
NoneLost == phase = "gone" => ended
=============================================================================

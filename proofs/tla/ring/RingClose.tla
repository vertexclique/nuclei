------------------------------ MODULE RingClose ------------------------------
(***************************************************************************)
(* A ring source dropped with an operation out (src/io/source/ring.rs:     *)
(* `Source::detach_ring`; src/rt/shard/ring.rs: `ShardRing::detach`,       *)
(* `take_out`, `finish_leaving`; src/rt/park/ring.rs:                      *)
(* `RingPoller::submit_now`; src/io/source/requests.rs: the close and its  *)
(* lease; src/syscore/linux/iouring/files.rs: the slot).                   *)
(*                                                                          *)
(* Who acts. The handle, dropped with an accept out on its shard's ring,   *)
(* on the ring's own thread (`Here`) or on another thread. The ring's      *)
(* thread: its waits hand what its submission queue holds to the kernel,   *)
(* apply its mailbox, and take the completions. Someone opening a file,    *)
(* who gets the descriptor's number once it is free.                       *)
(*                                                                          *)
(* The source's slot is a fixed file of the ring's table (`Table`), which  *)
(* entries name by index, or names the handle's own descriptor, by number: *)
(* the ring makes no copy of it. The accept starts queued (in the          *)
(* submission queue) or in the kernel; once in the kernel it holds the     *)
(* file itself.                                                            *)
(*                                                                          *)
(* What can happen. The detach cancels what is out, hands every entry      *)
(* naming the slot to the kernel at once (the cancel ending the accept in  *)
(* the same call), takes the file out of the table, and lets go of the     *)
(* slot, which stays reserved until the accept's completion was taken. The *)
(* kernel may hold completions it has no room for (`Busy`): then the       *)
(* submission waits for the ring's next wait, and so does the take-out. A  *)
(* lease keeps the handle's object open until then; on another thread the  *)
(* detach goes to the ring's mailbox with the lease.                       *)
(*                                                                          *)
(* What must never happen. An entry reaches the kernel naming a number    *)
(* that another file took; a slot is handed out while an entry names it.   *)
(* And on the ring's thread, with the kernel taking everything, the file  *)
(* is not closed when the drop returns (a listener would still accept).    *)
(*                                                                          *)
(* What is abstract. One source, one operation (the accept), one cancel;   *)
(* the kernel's task work, which the detach's call runs, ends the          *)
(* cancelled accept within it; a file is closed once nothing holds it (the *)
(* object, the table, a request in the kernel).                           *)
(*                                                                          *)
(* The kernel may refuse to take a fixed file out of the table once        *)
(* (`UpdateFails`: `IORING_REGISTER_FILES_UPDATE2` finds no memory). Then  *)
(* the file stays in the table, so the kernel keeps it open, and the slot  *)
(* stays held: handed to another source, the slot's next take-out would    *)
(* take that source's file out instead. The take-out is tried again after *)
(* the ring's next wait (`take_out` puts the slot back on `leaving`), and  *)
(* a short timer the refusal arms makes that wait come though nothing else *)
(* is out (`RETRY_TAKE_OUT`): an idle ring's thread would sleep for good.  *)
(* The lease goes all the same: the object may close, as the table holds   *)
(* the file meanwhile.                                                     *)
(***************************************************************************)
EXTENDS Integers, TLC                                                        \* numbers and TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Table,     \* TRUE: the slot is a fixed file of the ring's table; FALSE: it names the handle's descriptor
    Here,      \* TRUE: the handle drops on the ring's own thread; FALSE: on another thread
    Busy,      \* TRUE: the kernel holds completions it has no room for as the drop comes
    UpdateFails, \* TRUE: the kernel refuses the first take-out of a fixed file from the table
    Mutation   \* "none", or the name of the one rule this run takes away

(* --algorithm RingClose                                                      \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    objOpen = TRUE,                                                          \* the handle's object, its descriptor, is open
    tableHas = Table,                                                        \* the ring's table holds the file
    entry \in {"queued", "kernel"},                                          \* the accept: queued, in the kernel, or "ended"
    named = TRUE,                                                            \* the slot's count of entries: its end not taken yet
    cancelling = FALSE,                                                      \* the detach cancelled what is out
    lease = FALSE,                                                           \* a lease keeps the object open for the ring
    detachPosted = FALSE,                                                    \* the detach waits in the ring's mailbox
    leaving = FALSE,                                                         \* the detach is to be finished after the next wait
    retry = FALSE,                                                           \* a take-out the kernel refused waits to be tried again
    timer = FALSE,                                                           \* the retry's timer is armed: the ring's thread waits again
    refusals = IF UpdateFails THEN 1 ELSE 0,                                 \* how many take-outs the kernel may still refuse
    refused = FALSE,                                                         \* (record) the kernel refused a take-out
    slotHeld = TRUE,                                                         \* the source holds the slot
    slotFree = FALSE,                                                        \* the slot is free to hand out again
    reused = FALSE,                                                          \* another file took the descriptor's number
    dropDone = FALSE,                                                        \* the handle's drop returned
    strayed = FALSE,                                                         \* (record) an entry reached the kernel on a number another file took
    leftOnce = FALSE;                                                        \* (record) a detach was left to the next wait

define                                                                       \* names the properties use:
    \* What holds the file: the object, the table, the accept in the kernel.
    Refs == (IF objOpen THEN 1 ELSE 0) + (IF tableHas THEN 1 ELSE 0)        \* the object and the table
          + (IF entry = "kernel" THEN 1 ELSE 0)                              \* and the request
    \* Whether the detach hands everything to the kernel at once.
    \* MUTATION no_submit: everything waits for the ring's next wait, as before the fix.
    AtOnce == Mutation # "no_submit" /\ ~Busy                                \* the kernel takes it all now
    \* The table after the take-out.
    \* MUTATION table_until_done: the file stays in the table until the slot frees.
    TakenOut == IF Mutation = "table_until_done" THEN tableHas ELSE FALSE    \* out at once
    \* The slot after the release: free if no entry names it, else reserved until its end.
    \* MUTATION free_at_release: the slot is free at once, though an entry names it.
    FreedAt == slotFree \/ ~named \/ Mutation = "free_at_release"           \* reserved until its end
    \* The object once the close's hold went, while the ring still has the detach to finish.
    \* MUTATION close_before_flush: no lease: the object closes at once.
    KeptOpen == Mutation # "close_before_flush"                              \* the lease keeps it open
end define;                                                                  \* end of the names

\* The take-out, once every entry naming the slot reached the kernel (`take_out`): the file leaves the
\* table and the slot is released; or, a fixed file only, the kernel refuses (no memory).
macro TakeOut() begin                                                        \* one take-out, in the step that does it
    either                                                                   \* the kernel takes the file out:
        tableHas := TakenOut;                                                \* the file leaves the table
        slotHeld := FALSE;                                                   \* the slot is released
        slotFree := FreedAt;                                                 \* free if unnamed, else reserved while named
        retry := FALSE;                                                      \* nothing is left to try again
        timer := FALSE;                                                      \* and no timer is armed
    or                                                                       \* or it refuses:
        await Table /\ tableHas /\ refusals > 0;                              \* a fixed file, while it may still refuse
        refusals := refusals - 1;                                            \* it refuses this once
        refused := TRUE;                                                     \* (recorded)
        if Mutation = "update_ignored" then                                  \* MUTATION update_ignored: the refusal is ignored, as before the fix:
            slotHeld := FALSE;                                               \* the slot is released, the file still in the table
            slotFree := FreedAt;                                             \* and nothing tries again
        elsif Mutation = "update_frees" then                                 \* MUTATION update_frees: tried again later, but
            slotHeld := FALSE;                                               \* the slot is released now, the file still in it
            slotFree := FreedAt;                                             \* (another source could take it)
            retry := TRUE;                                                   \* the take-out waits to be tried again
            timer := TRUE;                                                   \* and its timer is armed
        else                                                                 \* the fix: the slot stays held, the file in it,
            retry := TRUE;                                                   \* and the take-out waits to be tried again
            \* MUTATION update_no_wake: no timer, so only other work makes the ring's thread wait again.
            timer := Mutation # "update_no_wake";                            \* the retry's timer is armed
        end if;                                                              \* the refusal is dealt with
    end either;                                                              \* taken out, or refused
end macro;                                                                   \* end of the take-out

\* The handle's drop: its close's detach, here or through the mailbox; then the close's hold goes.
fair process handle = "handle"                                               \* weakly fair: the drop comes
begin                                                                        \* the drop's steps:
H_drop:                                                                      \* one step: on the ring's thread nothing else runs meanwhile
    if Here then                                                             \* the ring's own thread:
        cancelling := TRUE;                                                  \* the accept's cancel is queued
        if AtOnce then                                                       \* the kernel takes every entry now:
            entry := "ended";                                                \* the accept goes in and its cancel ends it, its file let go
            TakeOut();                                                       \* the file leaves the table and the slot is released, or not
            objOpen := FALSE;                                                \* no lease is left: the object closes
        else                                                                 \* the kernel cannot take it all now:
            leaving := TRUE;                                                 \* the next wait finishes the detach
            leftOnce := TRUE;                                                \* (recorded)
            lease := KeptOpen;                                               \* a lease keeps the object open meanwhile
            objOpen := KeptOpen;                                             \* so the number stays the handle's
        end if;                                                              \* the detach is done or left
    else                                                                     \* another thread:
        detachPosted := TRUE;                                                \* the detach goes to the ring's mailbox
        lease := KeptOpen;                                                   \* with a lease on the object
        objOpen := KeptOpen;                                                 \* which stays open until the ring's thread is done
    end if;                                                                  \* the close's own hold has gone
    dropDone := TRUE;                                                        \* the drop returns
end process;                                                                 \* end of the handle

\* The ring's thread: its waits, its mailbox, its completions, until the source is all gone.
fair process ring = "ring"                                                   \* weakly fair: it keeps running
begin                                                                        \* the ring's steps:
R_loop:                                                                      \* the next thing to do
    while ~(dropDone /\ ~detachPosted /\ ~leaving /\ ~retry /\ ~named) do    \* until the source is all gone
        either                                                               \* a wait:
            await \/ entry = "queued" \/ (cancelling /\ entry = "kernel") \/ leaving  \* something to submit or finish,
                  \/ (retry /\ timer);                                        \* or the retry's timer ran out
            \* What the queue holds goes to the kernel, which takes it all (the wait took its completions).
            if entry = "queued" /\ ~Table /\ reused then strayed := TRUE; end if;    \* a queued entry on a number gone to another file
            entry := IF cancelling /\ entry # "ended" THEN "ended"           \* a cancelled accept ends
                     ELSE IF entry = "queued" THEN "kernel" ELSE entry;      \* else a queued one goes in
            if leaving \/ retry then                                         \* a detach left to this wait, or a refused take-out:
                TakeOut();                                                   \* the file leaves the table and the slot is released, or not
                lease := FALSE;                                              \* the lease goes
                objOpen := FALSE;                                            \* and with the close's hold gone, the object closes
                leaving := FALSE;                                            \* the detach is finished, but for a take-out to try again
            end if;                                                          \* the wait is done
        or                                                                   \* the mailbox:
            await detachPosted;                                              \* a detach posted from another thread
            detachPosted := FALSE;                                           \* taken
            cancelling := TRUE;                                              \* the accept's cancel is queued
            if AtOnce then                                                   \* the kernel takes every entry now:
                if entry = "queued" /\ ~Table /\ reused then strayed := TRUE; end if;  \* a queued entry on a number gone elsewhere
                entry := "ended";                                            \* the accept goes in and its cancel ends it
                TakeOut();                                                   \* the file leaves the table and the slot is released, or not
                lease := FALSE;                                              \* the lease goes
                objOpen := FALSE;                                            \* and the object closes
            else                                                             \* the kernel cannot take it all now:
                leaving := TRUE;                                             \* the next wait finishes it
                leftOnce := TRUE;                                            \* (recorded)
            end if;                                                          \* the detach is done or left
        or                                                                   \* a completion:
            await entry = "ended" /\ named;                                  \* the accept's end came
            named := FALSE;                                                  \* no entry names the slot now
            if ~slotHeld then slotFree := TRUE; end if;                      \* released and unnamed: free
            if Table /\ Mutation = "table_until_done" /\ ~slotHeld then      \* (the mutation's late take-out)
                tableHas := FALSE;                                           \* the file leaves the table only now
            end if;                                                          \* the end is taken
        end either;                                                          \* one of the three
    end while;                                                               \* the source is all gone
end process;                                                                 \* end of the ring's thread

\* Someone opening a file: it gets the descriptor's number once the object closed.
fair process opener = "opener"                                               \* weakly fair
begin                                                                        \* its one step:
O_open:                                                                      \* a file is opened
    await ~objOpen;                                                          \* the number is free
    either reused := TRUE; or skip; end either;                              \* it may get it, or another
end process;                                                                 \* end of the opener
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* No entry ever reaches the kernel naming a number another file took.
NoStray == ~strayed                                                          \* every entry reaches its own source's file

\* A slot is free to hand out only once no entry names it.
SlotReserved == slotFree => ~named                                           \* reserved while named

\* Dropped on the ring's thread with the kernel taking everything, the file is closed when the drop returns
\* (unless the kernel refused to take it out of the table, which AllClosed then covers).
ClosedAtDrop == (Here /\ ~Busy /\ ~refused /\ dropDone) => Refs = 0          \* nothing holds it then

\* A slot free to hand out holds no file in the table: its next source's take-out takes out only its own.
SlotEmpty == slotFree => ~tableHas                                           \* handed back empty

\* In the end the file is closed and the slot free, wherever the drop came from.
AllClosed == <>(Refs = 0 /\ slotFree)                                        \* closed, and the slot handed back

\* `Termination` (every process ends) is the one the translator writes for the fair processes.

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A detach is left to the ring's next wait (the kernel could not take it all).
NoLeaving == ~leftOnce                                                       \* never left
\* Another file takes the descriptor's number once the object closed.
NoReuse == ~reused                                                           \* never taken
\* The kernel refuses to take the file out of the table, and the take-out is tried again.
NoRefusal == ~(refused /\ retry)                                            \* never refused and retried
=============================================================================

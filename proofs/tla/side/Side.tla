------------------------------- MODULE Side -------------------------------
(***************************************************************************)
(* The guard of one side of an IO source, shared by a poll and the shard's *)
(* driver (src/io/side.rs, src/io/source/read.rs), at the grain of its     *)
(* atomic steps.                                                           *)
(*                                                                          *)
(* In plain words: the side's word has a BUSY bit (someone holds the side *)
(* for one step), a READY bit (an edge came while it was held) and a      *)
(* WAITING bit (a poll found it held and waits to be woken). Nobody ever   *)
(* waits for the holder. Bytes arrive in the kernel, each with an edge for *)
(* the driver, which takes the side with one fetch_or of BUSY and READY:   *)
(* free, it holds it and serves the edge (the cache is cleared, and with  *)
(* a reader waiting it reads what the kernel has into the read-ahead and  *)
(* wakes the reader, or marks the cache blocked); held, the READY it left  *)
(* is the holder's to serve. The reader takes the side with one fetch_or   *)
(* of BUSY: free, it takes the read-ahead, or (unless the cache says the  *)
(* call would block) runs the driver's read at once into the read-ahead   *)
(* and takes that, or registers its waker. Held, it stores its waker in   *)
(* the contended set and raises WAITING; if that showed the side free, it *)
(* takes it once more, else it sleeps until the holder wakes it. Letting  *)
(* go is one compare-and-swap from the word the holder took to zero; when  *)
(* flags came meanwhile it fails, and the holder takes them with one swap *)
(* (still holding): READY, it does the driver's part (and its own step    *)
(* again if it waited); WAITING, it wakes the contended reader. Then it    *)
(* tries again.                                                            *)
(*                                                                          *)
(* What is modelled step by step: every access to the word (the fetch_or  *)
(* takes, the release's compare-and-swap and swap), the read system call  *)
(* apart from what the holder does with its answer, the arrival of bytes, *)
(* the registration in the contended set, the wakes and the sleeps.        *)
(*                                                                          *)
(* What is abstract: bytes are a count; one reader (several tasks waiting *)
(* on one side share its waker list, which the driver wakes whole).       *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Total,     \* the bytes that arrive, one at a time
    Mutation   \* "none", or the name of the one rule this run takes away

(* --algorithm Side
variables
    busy = FALSE,       \* the side's word: BUSY ...
    ready = FALSE,      \* ... READY ...
    waiting = FALSE,    \* ... and WAITING
    kernel = 0,         \* bytes the kernel holds
    edges = 0,          \* edges not yet handed to the driver
    arrived = 0,        \* bytes that arrived
    blocked = TRUE,     \* the readiness cache: the last read would have blocked
    readerWaits = FALSE,  \* the reader's waker is registered for bytes
    readerWoken = FALSE,  \* ... and it was woken
    contended = FALSE,  \* the reader's waker is in the contended set
    contendWoken = FALSE, \* ... and the holder woke it
    ahead = 0,          \* bytes the driver read for the reader
    got = 0,            \* bytes the reader took
    flagged = FALSE,    \* (record) the driver flagged a held side
    delegated = FALSE,  \* (record) the reader did the driver's part
    driverRead = FALSE, \* (record) the driver read for the reader
    submitRead = FALSE, \* (record) a reader's submit ran the driver's read
    wokenByHolder = FALSE, \* (record) the holder woke a contended reader
    retook = FALSE;     \* (record) a contended reader found the side free and took it

define
    AllDone == pc["reader"] = "Done" /\ pc["kernel"] = "Done"
end define;

\* The driver's part, by whoever holds the side: an edge clears the cache; with the reader waiting,
\* bytes read ahead (or read now) wake it, and an empty kernel marks the cache blocked.
macro Serve(who) begin
    \* MUTATION edge_keeps_cache: an edge leaves the cache as it was.
    if readerWaits /\ (Mutation # "edge_keeps_cache" \/ ~blocked) then
        if ahead > 0 then
            readerWoken := TRUE; readerWaits := FALSE;
            if Mutation # "edge_keeps_cache" then blocked := FALSE; end if;
        elsif kernel > 0 then
            ahead := ahead + kernel; kernel := 0;
            if who = "driver" then driverRead := TRUE; end if;
            readerWoken := TRUE; readerWaits := FALSE;
            if Mutation # "edge_keeps_cache" then blocked := FALSE; end if;
        else
            blocked := TRUE;
        end if;
    elsif Mutation # "edge_keeps_cache" then
        blocked := FALSE;
    end if;
end macro;

fair process kernelP = "kernel"
begin
K_arrive:
    while arrived < Total do
        kernel := kernel + 1;
        arrived := arrived + 1;
        edges := edges + 1;
    end while;
end process;

fair process driver = "driver"
variables took = FALSE, owed = FALSE;
begin
D_loop:
    while TRUE do
        await edges > 0 \/ (AllDone /\ edges = 0);
        if edges = 0 then goto Done; end if;
D_take:
        edges := edges - 1;
D_try:
        \* One fetch_or of BUSY | READY: free, the driver holds the side (its own READY is masked,
        \* since only the driver raises READY); held, the READY is the holder's to serve.
        if ~busy then
            busy := TRUE; took := TRUE;
        else
            took := FALSE;
            if Mutation = "skip_held" then
                \* MUTATION skip_held: a held side makes the driver drop the edge.
                skip;
            elsif Mutation = "flag_in_two_steps" then
                \* MUTATION flag_in_two_steps: the driver looked first and flags in a step of its own.
                goto D_flag_late;
            else
                ready := TRUE; flagged := TRUE;
            end if;
        end if;
D_check:
        if ~took then goto D_loop; end if;
D_serve:
        Serve("driver");
D_cas:
        \* Let go: one compare-and-swap from BUSY (its own READY masked) to zero.
        if ~waiting then
            busy := FALSE;
            goto D_deliver;
        else
            waiting := FALSE;
        end if;
D_wake:
        \* The contended wakers are taken now, and woken once the side is let go.
        \* MUTATION release_ignores_waiting: the holder takes WAITING and wakes nobody.
        if contended /\ Mutation # "release_ignores_waiting" then
            contended := FALSE; owed := TRUE;
        end if;
        goto D_cas;
D_deliver:
        if owed then
            contendWoken := TRUE; wokenByHolder := TRUE; owed := FALSE;
        end if;
        goto D_loop;
D_flag_late:
        ready := TRUE; flagged := TRUE;
    end while;
end process;

fair process reader = "reader"
variables out = "pending", prevBusy = FALSE;
begin
R_loop:
    while got < Total do
R_try:
        \* One fetch_or of BUSY.
        \* MUTATION take_needs_zero: a side carrying a flag is taken for held.
        if ~busy /\ (Mutation # "take_needs_zero" \/ (~ready /\ ~waiting)) then
            busy := TRUE;
            goto R_step;
        end if;
R_register:
        \* Held: the waker goes to the contended set first.
        contended := TRUE;
R_flag:
        \* One fetch_or of WAITING, which says whether the side was still held.
        prevBusy := busy;
        waiting := TRUE;
R_recheck:
        \* MUTATION contender_no_recheck: the reader sleeps without looking at what the fetch_or said.
        if prevBusy \/ Mutation = "contender_no_recheck" then
            goto R_park;
        elsif ~busy /\ (Mutation # "take_needs_zero" \/ (~ready /\ ~waiting)) then
            busy := TRUE; retook := TRUE;
            goto R_step;
        else
            goto R_park;
        end if;
R_park:
        await contendWoken;
        contendWoken := FALSE;
        goto R_try;
R_step:
        if ahead > 0 then
            got := got + ahead; ahead := 0; out := "ready";
            goto R_cas;
        elsif blocked then
            readerWaits := TRUE; out := "pending";
            goto R_cas;
        end if;
R_read:
        \* The driver's read, run by the reader's submit: into the read-ahead.
        if kernel > 0 then
            ahead := ahead + kernel; kernel := 0;
            submitRead := TRUE;
            goto R_take;
        end if;
R_mark:
        \* It would have blocked: the cache, and the waker.
        blocked := TRUE; readerWaits := TRUE; out := "pending";
        goto R_cas;
R_take:
        \* The reader copies out what the driver read.
        got := got + ahead; ahead := 0; out := "ready";
R_cas:
        \* Let go: one compare-and-swap from BUSY to zero; flags make it fail.
        if ~ready /\ ~waiting then
            busy := FALSE;
            goto R_after;
        end if;
R_ready:
        \* The flags, taken with one swap back to BUSY: READY is the driver's part, done here.
        \* MUTATION release_ignores_ready: READY is taken and nothing done for it.
        if ready then
            ready := FALSE;
            if Mutation # "release_ignores_ready" then
                Serve("reader");
                delegated := TRUE;
            end if;
        end if;
R_wake:
        if waiting then
            waiting := FALSE;
            if contended /\ Mutation # "release_ignores_waiting" then
                contendWoken := TRUE; contended := FALSE; wokenByHolder := TRUE;
            end if;
        end if;
        if out = "pending" then goto R_step; else goto R_cas; end if;
R_after:
        if out = "pending" then
R_sleep:
            await readerWoken;
            readerWoken := FALSE;
        end if;
    end while;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* No byte is lost or made up: each is in the kernel, read ahead, or taken.
Conservation == got + ahead + kernel = arrived

\* One holder at a time, and the word says so.
DriverHolds == pc["driver"] \in {"D_serve", "D_cas", "D_wake"}
ReaderHolds == pc["reader"] \in {"R_step", "R_read", "R_mark", "R_take", "R_cas", "R_ready", "R_wake"}
Exclusive == ~(DriverHolds /\ ReaderHolds) /\ ((DriverHolds \/ ReaderHolds) => busy)

\* `Termination` (every thread finishes, so every byte reached the reader and no wake was lost) is
\* the one the translator writes for the fair processes.

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* The driver finds the side held and flags it.
NoFlag == ~flagged
\* The reader, letting go, does the driver's part.
NoDelegate == ~delegated
\* The driver completes a read for a waiting reader.
NoDriverRead == ~driverRead
\* A reader's submit runs the driver's read and takes its bytes.
NoSubmitRead == ~submitRead
\* A reader that found the side held is woken by the holder as it lets go.
NoHolderWake == ~wokenByHolder
\* A reader that found the side held finds it free after raising WAITING and takes it.
NoRetake == ~retook
=============================================================================

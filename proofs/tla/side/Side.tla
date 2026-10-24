------------------------------- MODULE Side -------------------------------
(***************************************************************************)
(* The guard of one side of an IO source, shared by a poll and the shard's *)
(* driver (src/io/side.rs, src/io/source/read.rs), at the grain of its     *)
(* atomic steps.                                                           *)
(*                                                                          *)
(* In plain words: the side's word has a BUSY bit (someone holds the side *)
(* for one step) and a READY bit (an edge came while it was held). Bytes   *)
(* arrive in the kernel, each with an edge for the driver. The driver      *)
(* takes a free side and serves it: an edge clears the readiness cache,    *)
(* and with a reader waiting it reads what the kernel has into the read-   *)
(* ahead and wakes the reader, or marks the cache blocked. Finding the     *)
(* side held, it sets READY instead and goes on; if the holder let go      *)
(* before the flag landed, it takes the side after all. The reader holds   *)
(* the side for one step: it takes the read-ahead, or reads the kernel     *)
(* itself unless the cache says it would block, or registers its waker.    *)
(* As it lets go it looks at READY: if set, it takes the side again and    *)
(* does the driver's part, then its own step again if it was waiting.      *)
(*                                                                          *)
(* What is modelled step by step: every access to the word (the hold's     *)
(* compare-and-swap, the flag's fetch-or, the release's swap), the read    *)
(* system call apart from what the holder does with its answer, the       *)
(* arrival of bytes, the wake and the reader's sleep.                      *)
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
    state = 0,          \* the side's word: BUSY = 1, READY = 2
    kernel = 0,         \* bytes the kernel holds
    edges = 0,          \* edges not yet handed to the driver
    arrived = 0,        \* bytes that arrived
    blocked = FALSE,    \* the readiness cache: the last read would have blocked
    waiting = FALSE,    \* the reader's waker is registered
    woken = FALSE,      \* the reader was woken and has not looked yet
    ahead = 0,          \* bytes the driver read for the reader
    got = 0,            \* bytes the reader took
    flagged = FALSE,    \* (record) the driver flagged a held side
    delegated = FALSE,  \* (record) the reader did the driver's part
    driverRead = FALSE, \* (record) the driver read for the reader
    inline = FALSE;     \* (record) the reader read the kernel itself

define
    Busy(s) == s % 2 = 1
    Flag(s) == s \div 2 = 1
    \* MUTATION hold_needs_zero: a hold takes only a word with no flag on it.
    CanHold(s) == IF Mutation = "hold_needs_zero" THEN s = 0 ELSE ~Busy(s)
    AllDone == pc["reader"] = "Done" /\ pc["kernel"] = "Done"
end define;

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
variables edge = FALSE;
begin
D_loop:
    while TRUE do
        await edges > 0 \/ (AllDone /\ edges = 0);
        if edges = 0 then goto Done; end if;
D_take:
        edges := edges - 1;
        edge := TRUE;
D_try:
        \* `hold_or_flag`: the side if it is free ...
        if CanHold(state) then
            state := 1;
            goto D_edge;
        elsif Mutation = "skip_held" then
            \* MUTATION skip_held: a held side makes the driver drop the edge.
            goto D_loop;
        end if;
D_flag:
        \* ... else the flag, for the holder.
        flagged := TRUE;
        if Busy(state) then
            state := 3;
            goto D_loop;
        else
            \* The holder let go before the flag landed: take the side.
            state := 2;
            goto D_try;
        end if;
D_edge:
        \* MUTATION edge_keeps_cache: an edge leaves the cache as it was.
        if edge /\ Mutation # "edge_keeps_cache" then blocked := FALSE; end if;
D_serve:
        if ~blocked /\ waiting then
            if ahead > 0 then
                woken := TRUE; waiting := FALSE;
            elsif kernel > 0 then
                ahead := ahead + kernel; kernel := 0;
                driverRead := TRUE;
                woken := TRUE; waiting := FALSE;
            else
                blocked := TRUE;
            end if;
        end if;
D_release:
        \* A flag that landed while the driver held the side is an edge too.
        if Flag(state) then
            state := 0; edge := TRUE;
            goto D_try;
        else
            state := 0;
        end if;
    end while;
end process;

fair process reader = "reader"
variables out = "pending";
begin
R_loop:
    while got < Total do
R_hold:
        \* `hold`: waits for the holder's one step.
        await CanHold(state);
        state := 1;
R_step:
        if ahead > 0 then
            got := got + ahead; ahead := 0; out := "ready";
            goto R_release;
        elsif blocked then
            waiting := TRUE; out := "pending";
            goto R_release;
        end if;
R_read:
        \* The read system call, into the reader's own buffer.
        if kernel > 0 then
            got := got + kernel; kernel := 0;
            inline := TRUE; out := "ready";
            goto R_release;
        end if;
R_mark:
        \* It would have blocked: the cache, and the waker.
        blocked := TRUE; waiting := TRUE; out := "pending";
R_release:
        \* MUTATION release_ignores_ready: the reader lets go without looking.
        if Flag(state) /\ Mutation # "release_ignores_ready" then
            state := 0;
            delegated := TRUE;
            goto R_rehold;
        else
            state := 0;
            goto R_after;
        end if;
R_rehold:
        await CanHold(state);
        state := 1;
R_edge:
        \* The driver's part, for the edge that came meanwhile.
        blocked := FALSE;
R_serve:
        if waiting then
            if ahead > 0 then
                woken := TRUE; waiting := FALSE;
            elsif kernel > 0 then
                ahead := ahead + kernel; kernel := 0;
                woken := TRUE; waiting := FALSE;
            else
                blocked := TRUE;
            end if;
        end if;
R_again:
        if out = "pending" then goto R_step; else goto R_release; end if;
R_after:
        if out = "pending" then
R_sleep:
            await woken;
            woken := FALSE;
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
DriverHolds == pc["driver"] \in {"D_edge", "D_serve", "D_release"}
ReaderHolds == pc["reader"] \in {"R_step", "R_read", "R_mark", "R_release", "R_edge", "R_serve", "R_again"}
Exclusive == ~(DriverHolds /\ ReaderHolds) /\ ((DriverHolds \/ ReaderHolds) => Busy(state))

\* `Termination` (every thread finishes, so every byte reached the reader) is the one the
\* translator writes for the fair processes.

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
\* The reader reads the kernel itself.
NoInline == ~inline
=============================================================================

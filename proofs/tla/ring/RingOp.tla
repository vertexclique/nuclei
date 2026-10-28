------------------------------ MODULE RingOp ------------------------------
(***************************************************************************)
(* A side of a source on its shard's ring (src/io/source/write/ring.rs,   *)
(* src/io/source/ring.rs, src/rt/shard/ring.rs), with the side held from  *)
(* another thread, and a retire.                                          *)
(*                                                                          *)
(* In plain words: a task on another thread writes twice. Each write is   *)
(* accepted into the side; the side's operation goes to the ring, one at  *)
(* a time, and only the ring's own thread submits it: a task elsewhere    *)
(* posts a kick to the ring's thread, which submits what the side wants.  *)
(* Submitting raises the side's flag (an operation is out) and the       *)
(* source's count of operations in the kernel. When the kernel completes *)
(* one, the ring's thread lowers the count, leaves the completion in the *)
(* side's one-slot mailbox and defers it to the side: applied at once if *)
(* the side is free, else by its holder as it lets go. Applying it clears *)
(* the flag and asks for the next operation (submitted there on the       *)
(* ring's thread, kicked from elsewhere). A retire deregisters the source *)
(* (nothing is submitted after it) and cancels what is out, then waits    *)
(* until the count is zero: it puts its waker in, raises a flag, and only *)
(* then looks at the count; the ring's thread lowers the count, then looks *)
(* at the flag and wakes the waker it finds.                              *)
(*                                                                          *)
(* What is abstract: the side's guard, as an atomic take-or-defer (the     *)
(* side family checks it); the bytes (each write is one operation); the   *)
(* kernel (it completes an operation it holds, cancelled or not).          *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    Mutation,  \* "none", or the name of the one rule this run takes away
    Retires    \* whether a retire comes

Writes == 2

(* --algorithm RingOp
variables
    busy = FALSE,          \* the side is held
    deferred = {},         \* work left to the side's holder: "SUBMIT", "DONE"
    accepted = 0,          \* writes the side accepted
    sent = 0,              \* writes whose operation completed
    out = 0,               \* the side's operations out (only a completion lowers it)
    inKernel = 0,          \* operations the kernel holds
    count = 0,             \* the source's count of operations in the kernel's hands
    mailbox = 0,           \* completions left for the side
    kicks = 0,             \* kicks posted to the ring's thread
    kickLater = [p \in {"task", "ring", "retire"} |-> FALSE],  \* a kick to post once the side is let go
    detached = FALSE,      \* the retire deregistered the source
    awaits = FALSE,        \* the retirer's flag
    wakerIn = FALSE,       \* the retirer's waker is in
    woken = FALSE,         \* the retirer was woken
    overwrote = FALSE,     \* (record) a completion was left over another
    offThread = FALSE,     \* (record) an operation was submitted by a thread not the ring's
    taskApplied = FALSE,   \* (record) the task's thread applied a completion
    retireWaited = FALSE;  \* (record) the retirer waited for a completion

define
    \* What the side has for the ring: an accepted write not out yet. (Read in its own step: an
    \* operator sees the state the step started from.)
    Wants == accepted > sent + out
end define;

\* What the side's holder `who` does to submit what the side wants: the ring's thread submits it,
\* another thread posts a kick. Nothing after the retire deregistered the source.
macro SubmitWanted(who) begin
    \* MUTATION submit_while_out: a second operation goes out while one is.
    if Wants /\ ~detached /\ (out = 0 \/ Mutation = "submit_while_out") then
        \* MUTATION elsewhere_submits: the task's thread submits itself.
        if who = "ring" \/ Mutation = "elsewhere_submits" then
            out := out + 1;
            inKernel := inKernel + 1;
            count := count + 1;
            offThread := offThread \/ who # "ring";
        else
            \* Posted once the holder lets go of the side (the code's `settle`).
            kickLater[who] := TRUE;
        end if;
    end if;
end macro;

\* Applies a completion as holder `who`: the operation is no longer out.
macro ApplyDone(who) begin
    mailbox := mailbox - 1;
    out := out - 1;
    sent := sent + 1;
    taskApplied := taskApplied \/ who = "task";
end macro;

\* Lets go of the side as `who`: deferred work is done first, one bit at a time.
procedure Release(who)
variables bit = "none";
begin
Rel_loop:
    while deferred # {} do
        with b \in deferred do
            bit := b;
            deferred := deferred \ {b};
        end with;
Rel_apply:
        if bit = "DONE" then ApplyDone(who); end if;
Rel_submit:
        \* After a completion or a kick, what the side still wants goes to the ring.
        SubmitWanted(who);
    end while;
Rel_free:
    \* Letting go is one compare-and-swap from the word the holder has: work deferred meanwhile
    \* makes it fail, and the holder does that work first (the side family checks this).
    if deferred # {} then
        goto Rel_loop;
    else
        busy := FALSE;
    end if;
Rel_kick:
    \* MUTATION no_kick: elsewhere, the side's want is left for nobody.
    if kickLater[who] /\ Mutation # "no_kick" then
        kicks := kicks + 1;
    end if;
    kickLater[who] := FALSE;
    return;
end procedure;

\* The task, on another thread than the ring's: two writes, each accepted while it holds the side.
fair process task = "task"
variables w = 0;
begin
T_loop:
    while w < Writes do
T_take:
        await ~busy;
        busy := TRUE;
T_accept:
        accepted := accepted + 1;
        w := w + 1;
T_submit:
        SubmitWanted("task");
T_release:
        call Release("task");
    end while;
end process;

\* The ring's thread: kicks posted to it, and the kernel's completions.
fair process ring = "ring"
variables bit = "none";
begin
R_loop:
    while TRUE do
        either
            \* A kick: what the side wants is submitted, at once if the side is free.
            await kicks > 0;
            kicks := kicks - 1;
            bit := "SUBMIT";
        or
            \* The kernel completes an operation: the count goes down, then the flag is looked at.
            await inKernel > 0;
            inKernel := inKernel - 1;
            count := count - 1;
R_wake:
            \* MUTATION no_wake: the completion that empties the count wakes nobody.
            if count = 0 /\ awaits /\ wakerIn /\ Mutation # "no_wake" then
                woken := TRUE;
                wakerIn := FALSE;
            end if;
R_put:
            if mailbox > 0 then overwrote := TRUE; end if;
            mailbox := mailbox + 1;
            bit := "DONE";
        end either;
R_defer:
        \* The bit goes to the side: applied here if the side is free, else left to its holder.
        if ~busy then
            busy := TRUE;
            deferred := deferred \cup {bit};
R_as_holder:
            call Release("ring");
        else
            deferred := deferred \cup {bit};
        end if;
    end while;
end process;

\* A retire: it takes the side, deregisters the source and cancels what is out (the kernel then
\* completes it), lets go, and waits until the count is zero.
fair process retirer = "retire"
variables seen = 0;
begin
X_start:
    if ~Retires then goto Done; end if;
X_take:
    await ~busy;
    busy := TRUE;
X_detach:
    detached := TRUE;
X_release:
    call Release("retire");
X_register:
    \* MUTATION skip_wait: the retire returns as soon as its request is done.
    if Mutation = "skip_wait" then
        goto Done;
    else
        wakerIn := TRUE;
        \* MUTATION count_first: the count is looked at before the flag is raised.
        if Mutation = "count_first" then seen := count; end if;
    end if;
X_flag:
    awaits := TRUE;
X_look:
    if Mutation # "count_first" then seen := count; end if;
    if seen = 0 then goto Done; end if;
X_wait:
    retireWaited := TRUE;
    await woken;
    woken := FALSE;
    goto X_register;
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

\* A side has one operation out at most, and its mailbox one completion: none is left over another.
OneOut == out <= 1
NoOverwrite == ~overwrote

\* Only the ring's own thread submits (it is the ring's one issuer).
SingleIssuer == ~offThread

\* Once the retire returned, the kernel holds no operation of the source.
RetiredIdle == pc["retire"] = "Done" => inKernel = 0

\* The retire returns.
RetireReturns == <>(pc["retire"] = "Done")

\* With no retire, every write goes out.
WritesGo == <>[](sent = Writes)

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* The task's thread applies a completion as it lets go of the side it holds.
NoTaskApplies == ~taskApplied
\* The retire waits for a completion before it returns.
NoRetireWait == ~retireWaited
=============================================================================

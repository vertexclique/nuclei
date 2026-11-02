-------------------------------- MODULE Going --------------------------------
(***************************************************************************)
(* A shard that goes (src/rt/shard/going.rs) and the registration of one   *)
(* of its sources (`Source::detach` and `registration_gone` in             *)
(* src/io/source.rs, `evict_now` in src/io/source/requests.rs), at the     *)
(* grain of the steps that decide when the shard may look at what its      *)
(* ring has out and at what its mailbox got.                               *)
(*                                                                          *)
(* In plain words: shard A goes with the source registered with it and a   *)
(* receive of the source out on its ring. It evicts the source (at once,   *)
(* or, while another thread holds a side, by that holder as it lets go)    *)
(* and waits until the source is no longer registered with it; then it     *)
(* applies its mailbox, where the eviction left its detach: the cancel of  *)
(* the receive, and the source's file, which A's ring lets go of (with the *)
(* lease that keeps the handle's object open meanwhile). A holder ends the *)
(* registration in steps: its state goes, then the detach goes to A's      *)
(* mailbox, and only then its token and its shard, so A's wait sees the    *)
(* end only once the detach is there. A shard that goes by leaving (a      *)
(* thread's own, whose outermost block_on returned) runs again only at its *)
(* thread's next block_on: a detach that came after its look would wait    *)
(* there, its ring holding the file and the handle never closing. A task   *)
(* polling the source on shard B registers it                              *)
(* there as soon as the eviction is done, under a token of the same number *)
(* (every shard numbers its own), and its receive goes out once the side's *)
(* receive on A has ended. The wait checks the registration's shard with   *)
(* its token, so B's registration is not mistaken for A's. A ring attach   *)
(* that failed leaves its token behind, but not its shard.                 *)
(*                                                                          *)
(* What is modelled step by step: the registration's token, shard and      *)
(* state, the operation out and the ring it is on, A's mailbox, the        *)
(* holder's release, and B's registration and receive. What is abstract:   *)
(* the side's guard and its requests (a held side's eviction is performed  *)
(* as the holder lets go, the side family's protocol), and the ring's      *)
(* cancel and completion, which settle applies in one step (`Shard::leave` *)
(* settles only while its ring has operations out; or, on a ring           *)
(* another thread can reap, the ring's keeper: src/rt/shard/ring/keeper.rs; *)
(* LentZc.tla checks the keeper's part). An attach of the source still    *)
(* asked of A, which A's wait also waits for, is Pending.tla's.            *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Held,      \* another thread holds a side of the source as A goes
    Failed,    \* the source's registration with A was a ring attach that failed
    Poller,    \* a task on shard B polls the source
    Mutation   \* "none", or the name of the one rule this run takes away (or breaks)

(* --algorithm Going
variables
    regd = ~Failed,
    token = "t0",
    home = IF Failed THEN (IF Mutation = "keep_home" THEN "A" ELSE "none") ELSE "A",
    out = IF Failed THEN "none" ELSE "A",
    mailA = 0,              \* detaches left in A's mailbox, not applied yet
    fileA = ~Failed,        \* A's ring holds the source's file (a failed attach left none)
    held = Held,
    evictPending = FALSE,
    ending = FALSE,         \* the holder is ending the registration, both sides held
    heldEvicted = FALSE;    \* (record) the holder performed the eviction

define
    \* A's wait: the source still registered with A under A's token.
    RegisteredWithA == token = "t0" /\ (home = "A" \/ Mutation = "token_only")
    AllEnd == <>(\A p \in {"A", "H", "T"} : pc[p] = "Done")
    \* Once A has gone, no operation of the source is out on its ring.
    NoStrandedOp == pc["A"] = "Done" => out # "A"
    \* Once A has gone, its ring holds no file of the source and no detach
    \* waits in its mailbox: the handle's object can close.
    NoStrandedDetach == pc["A"] = "Done" => (~fileA /\ mailA = 0)
    \* Witnesses.
    NoCollision == ~(pc["A"] = "A_wait" /\ home = "B" /\ token = "t0")
    NoHolderEvict == ~heldEvicted
end define;

\* `evict_now` on A's own thread: the registration goes, and A detaches the
\* source (its operation out cancelled, its file let go) as it settles.
macro evictNow() begin
    if regd then
        regd := FALSE;
        token := "none";
        home := "none";
        if fileA then
            mailA := mailA + 1;
        end if;
    end if;
end macro;

fair process shard = "A"
begin
A_evict:
    \* `evict`: at once if no other thread holds a side, else by the holder.
    if held then
        evictPending := TRUE;
    else
        evictNow();
    end if;
A_wait:
    await ~RegisteredWithA \/ Mutation = "no_wait";
A_settle:
    \* The mailbox: a detach takes the file out of the ring and ends the
    \* operation out here.
    if mailA > 0 then
        mailA := 0;
        fileA := FALSE;
        if out = "A" then
            out := "none";
        end if;
    end if;
end process;

fair process holder = "H"
begin
H_release:
    \* The holder lets go of the side and performs what was asked meanwhile,
    \* holding both sides: the eviction's first step is the state.
    if held then
        held := FALSE;
        if evictPending /\ regd then
            evictPending := FALSE;
            heldEvicted := TRUE;
            ending := TRUE;
            regd := FALSE;
            \* The broken order: the token and the shard go with the state,
            \* before the detach is posted.
            if Mutation = "end_first" then
                token := "none";
                home := "none";
            end if;
        else
            evictPending := FALSE;
            goto Done;
        end if;
    else
        goto Done;
    end if;
H_post:
    \* The detach goes to A's mailbox (`detach_ring` off A's thread).
    if fileA then
        mailA := mailA + 1;
    end if;
H_name:
    \* `registration_gone`: the token and the shard go last, and the holder
    \* lets go of both sides.
    if Mutation # "end_first" then
        token := "none";
        home := "none";
    end if;
    ending := FALSE;
end process;

fair process poller = "T"
begin
T_start:
    if ~Poller then
        goto Done;
    end if;
T_register:
    \* Woken by the eviction, it registers the source on B, whose first token
    \* has the same number as A's: its attach takes both sides, so it comes
    \* after the holder let go of them.
    await ~regd /\ ~ending;
    regd := TRUE;
    token := "t0";
    home := "B";
T_issue:
    \* Its receive goes out once the side's operation out has ended.
    await out = "none";
    out := "B";
T_leave:
    \* The receive completes; B's block_on returns and B evicts the source.
    regd := FALSE;
    token := "none";
    home := "none";
    out := "none";
end process;
end algorithm; *)
=============================================================================

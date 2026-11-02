------------------------------- MODULE MeshEnd -------------------------------
(***************************************************************************)
(* A mesh party's receive over three peers, each channel looked at in its  *)
(* own step, with the peers' ends and the cooperative budget's refusal     *)
(* (src/sync/mesh.rs: `Party::poll_recv`, `Party::end`; each channel's     *)
(* `Chan::poll_recv` in src/sync/mpsc/chan.rs).                            *)
(*                                                                          *)
(* In plain words: a poll looks at the channels round robin from the one   *)
(* after the last that delivered, one channel at a time. A channel with a *)
(* value gives it and the poll answers; one whose senders are all gone and *)
(* which is empty has ended and is skipped from then on; any other         *)
(* registers the party's waker. With every channel ended the poll answers  *)
(* the end; else it sleeps until a send, or a peer's last drop, wakes it.  *)
(* A poll whose task spent its budget takes nothing, wakes its own task    *)
(* and answers Pending (`coop::exhausted`), before it looks at any channel.*)
(*                                                                          *)
(* Who acts: the party, and three peers that send a value or two (into    *)
(* channels of one slot) and drop their sender. What must never happen:   *)
(* the end answered while a channel still has a value or a live sender; a *)
(* party asleep for good with a value or an end to see.                    *)
(*                                                                          *)
(* What is abstract: each channel is a count, a sender-gone flag and a     *)
(* registration flag, and a channel's own register-then-look-again is one  *)
(* step (the mpsc models check it); the round robin's fairness is          *)
(* Mesh.tla's.                                                             *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Sends,     \* per peer, how many values it sends before it drops its sender
    Mutation   \* "none", or the name of the one rule this run takes away

Peers == DOMAIN Sends
N == Cardinality(Peers)
PartyT == 0
Total == LET S[i \in 0..N] == IF i = 0 THEN 0 ELSE S[i - 1] + Sends[i] IN S[N]

(* --algorithm MeshEnd
variables
    queued = [p \in Peers |-> 0],          \* values in each peer's channel
    gone = [p \in Peers |-> FALSE],        \* each peer's sender dropped
    registered = [p \in Peers |-> FALSE],  \* the party's waker is in that channel
    woken = FALSE,                         \* the party's waker was called
    ended = [p \in Peers |-> FALSE],       \* the party saw that channel end
    received = 0,                          \* values the party took
    refusals = 0,                          \* polls the budget refused (a fresh budget follows)
    earlyEnd = FALSE;                      \* (record) the end answered too soon

define
    \* The end is answered only once every channel is empty with its sender gone.
    NoEarlyEnd == ~earlyEnd
    \* The party takes every value and then sees the end.
    Ends == <>(pc[PartyT] = "Done" /\ received = Total)
end define;

macro wake(p) begin
    if registered[p] then
        registered[p] := FALSE;
        woken := TRUE;
    end if;
end macro;

fair process peer \in Peers
variables k = 0;
begin
S_send:
    while k < Sends[self] do
        await queued[self] = 0;            \* one slot: a send waits for room
        queued[self] := 1;
        wake(self);
        k := k + 1;
    end while;
S_drop:
    \* The last sender's drop ends the channel and wakes the receiver.
    gone[self] := TRUE;
    wake(self);
end process;

fair process party = PartyT
variables next = 1, step = 0, from = 1;
begin
P_poll:
    \* `coop::exhausted`: a spent budget refuses the poll, waking its own task.
    \* MUTATION refuse_no_wake: the refusal wakes nobody.
    either
        await refusals < 2;                \* the next poll after a yield has a fresh budget
        refusals := refusals + 1;
        if Mutation # "refuse_no_wake" then
            woken := TRUE;
        end if;
        goto P_sleep;
    or
        step := 0;
    end either;
P_look:
    \* One channel's poll, round robin from `next`.
    if step < N then
        from := ((next - 1 + step) % N) + 1;
        if ended[from] then
            step := step + 1;
            goto P_look;
        elsif queued[from] > 0 then
            queued[from] := 0;
            received := received + 1;
            next := (from % N) + 1;
            goto P_poll;
        elsif gone[from] then
            ended[from] := TRUE;
            step := step + 1;
            goto P_look;
        else
            registered[from] := TRUE;
            step := step + 1;
            goto P_look;
        end if;
    end if;
P_end:
    \* Every channel ended: the end. Else sleep.
    \* MUTATION end_on_one: the end once any channel ended.
    if (\A p \in Peers : ended[p]) \/ (Mutation = "end_on_one" /\ \E p \in Peers : ended[p]) then
        earlyEnd := earlyEnd \/ \E p \in Peers : queued[p] > 0 \/ ~gone[p];
        goto Done;
    end if;
P_sleep:
    await woken;
    woken := FALSE;
    goto P_poll;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

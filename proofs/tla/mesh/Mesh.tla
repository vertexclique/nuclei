-------------------------------- MODULE Mesh --------------------------------
(***************************************************************************)
(* A party of a mesh receiving from any party (src/sync/mesh.rs:          *)
(* `Party::poll_recv`, `Party::recv`), over the bounded channels of        *)
(* src/sync/mpsc, at the grain of one poll of the party and one send.      *)
(*                                                                         *)
(* In plain words: a party has one channel from each other party. Its      *)
(* receive looks at them round robin, starting after the one that          *)
(* delivered last, takes the first value it finds, and remembers where it  *)
(* took it from; finding none, it has registered its waker with every one  *)
(* of them (each channel's own receive does), and sleeps until a send      *)
(* wakes it. One peer floods its channel, always refilling it; the other   *)
(* sends one value.                                                        *)
(*                                                                         *)
(* What must never happen: the quiet peer's value waiting while the party  *)
(* takes more than one round of values from the flooding one, or the party *)
(* asleep while a value waits for it.                                      *)
(*                                                                         *)
(* What is abstract: each channel is a count of values (its own steps, and *)
(* its register-then-look-again handshake, are the mpsc models'), a value  *)
(* has no content, and the flooding peer refills at once whenever its      *)
(* channel has room for one more.                                          *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    Floods,    \* how many values the flooding peer sends
    Mutation   \* "none", or the name of the one rule this run takes away

\* The peers, as the party's indexes: 1 floods, 2 sends one value.
Peers == 1..2
\* Each channel holds at most one value: the flooding peer is always one value ahead.
Cap == 1
\* The values the party takes in all.
Total == Floods + 1

(* --algorithm Mesh
variables
    queued = [p \in Peers |-> 0],          \* values in the channel from each peer
    registered = [p \in Peers |-> FALSE],  \* the party's waker is in that channel
    asleep = FALSE,                        \* the party's receive returned Pending and waits for a wake
    next = 1,                              \* where the next receive starts looking
    taken = 0,                             \* values the party took
    quietSent = FALSE,                     \* the quiet peer sent its value
    sinceQuiet = 0,                        \* values the party took while the quiet value waited
    slept = FALSE;                         \* (record) the party slept and was woken by a send

define
    \* The first peer, from `next` on round robin, whose channel has a value.
    Order == <<next, 1 + (next % 2)>>
    Found == {i \in 1..2 : queued[Order[i]] > 0}
    First == Order[CHOOSE i \in Found : \A j \in Found : i <= j]
end define;

\* A send to the party: the value goes in, and the party is woken if it registered there.
macro Send(p) begin
    queued[p] := queued[p] + 1;
    if registered[p] /\ asleep then
        asleep := FALSE;
        slept := TRUE;
    end if;
    registered[p] := FALSE;
end macro;

\* The flooding peer: it refills its channel as soon as there is room, `Floods` times.
fair process flooder = "flooder"
variables sent = 0;   \* values it sent
begin
F_send:
    while sent < Floods do
        await queued[1] < Cap;
        Send(1);
        sent := sent + 1;
    end while;
end process;

\* The quiet peer: it sends one value, at any moment.
fair process quiet = "quiet"
begin
Q_send:
    await queued[2] < Cap;
    Send(2);
    quietSent := TRUE;
end process;

\* The party: it receives until it took every value.
fair process party = "party"
begin
P_recv:
    while taken < Total do
        if Found # {} then
            \* A value: taken from the first channel round robin, and the next receive starts
            \* after it. MUTATION fixed_start: every receive starts at the first channel.
            with f = First do
                queued[f] := queued[f] - 1;
                taken := taken + 1;
                if Mutation # "fixed_start" then
                    next := 1 + (f % 2);
                end if;
                \* The count of flooded values taken while the quiet one waits.
                if quietSent /\ queued[2] > 0 /\ f # 2 then
                    sinceQuiet := sinceQuiet + 1;
                elsif f = 2 then
                    sinceQuiet := 0;
                end if;
            end with;
        else
            \* Nothing anywhere: every channel's receive registered the waker, and the party
            \* sleeps. MUTATION register_first_only: only the first channel looked at keeps it.
            if Mutation = "register_first_only" then
                registered[next] := TRUE;
            else
                registered := [p \in Peers |-> TRUE];
            end if;
            asleep := TRUE;
P_sleep:
            await ~asleep;
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

\* While the quiet value waits, the party takes at most one value from the other peer before it.
QuietServed == sinceQuiet <= 1

\* The party never sleeps while a value waits for it in a channel it registered with, nor with no
\* registration where a value can still come.
NoLostWake == asleep => \A p \in Peers : queued[p] = 0

(***************************************************************************)
(* Witnesses.                                                              *)
(***************************************************************************)

\* The party slept and a send woke it.
NoSleep == ~slept
\* The party took a flooded value while the quiet one waited.
NoOvertake == sinceQuiet = 0
=============================================================================

----------------------------- MODULE WakerSet -----------------------------
(***************************************************************************)
(* The contended set of a side's guard (src/io/side.rs: `WakerSet`,        *)
(* `register` and `take_all`, `SIDE_SLOTS`; the polls that leave a waker   *)
(* there, `Side::contend`, and the holder's wake as it lets go,            *)
(* `Side::wake_contended`).                                                *)
(*                                                                          *)
(* Who acts. Several tasks poll one side; whoever holds it holds it for a  *)
(* step and lets go. A poll that finds the side held keeps its waker in   *)
(* the set: in its own slot if it has one, else in a free slot, else, with *)
(* every slot holding another task's, in the last slot, and the waker it   *)
(* displaces is woken at once. It then looks once more (the side may have *)
(* come free meanwhile, as `contend`'s look at the word says) and sleeps   *)
(* until woken. The holder, letting go, wakes every waker in the set and   *)
(* frees the slots.                                                        *)
(*                                                                          *)
(* What can happen. With more tasks than slots, the displaced task polls  *)
(* again, finds the side still held and displaces another, which does the *)
(* same, for as long as the holder holds: their wakes go back and forth,   *)
(* which costs only time. What must happen: every task takes the side in   *)
(* the end (`Termination`); a displaced waker that were not woken would    *)
(* sleep for good.                                                        *)
(*                                                                          *)
(* What is abstract. Each task takes the side once and is done; a hold is *)
(* one step; the set's tags and wakers are one slot each, a registration  *)
(* and a take one step each (the side's word and its flags are            *)
(* Side.tla's).                                                            *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC                                            \* numbers, set sizes, TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Readers,   \* the tasks that poll the side
    Slots,     \* the slots of the contended set (`SIDE_SLOTS` is four)
    Mutation   \* "none", or the name of the one rule this run takes away

Rs == 1..Readers                                                             \* the tasks, by number
Ss == 1..Slots                                                               \* the slots, by number

(* --algorithm WakerSet
variables
    holder = 0,                   \* the task that holds the side; 0: nobody
    slot = [i \in Ss |-> 0],      \* the task whose waker each slot keeps; 0: empty
    woken = {},                   \* tasks woken and not polled again yet
    done = [r \in Rs |-> FALSE],  \* the task took the side
    displaced = FALSE;            \* (record) a registration displaced another task's waker

fair process reader \in Rs
begin
R_try:
    while ~done[self] do
        if holder = 0 then                                                   \* the side is free:
            holder := self;                                                  \* taken
            goto R_release;
        end if;
R_register:
        \* Its own slot, else a free one, else the last, waking the waker it displaces.
        if \E i \in Ss : slot[i] = self then                                 \* kept already
            skip;
        elsif \E i \in Ss : slot[i] = 0 then                                 \* a free slot:
            slot[CHOOSE i \in Ss : slot[i] = 0 /\ \A j \in Ss : slot[j] = 0 => i <= j] := self;
        else                                                                 \* every slot holds another's:
            \* MUTATION displace_no_wake: the displaced waker is dropped, not woken.
            if Mutation # "displace_no_wake" then
                woken := woken \cup {slot[Slots]};                           \* woken at once
            end if;
            slot[Slots] := self;                                             \* the last slot takes it
            displaced := TRUE;
        end if;
R_recheck:
        if holder = 0 then                                                   \* the side came free meanwhile:
            holder := self;                                                  \* taken after all
            goto R_release;
        end if;
R_sleep:
        await self \in woken;                                                \* until woken
        woken := woken \ {self};                                             \* it polls again
    end while;
    goto Done;
R_release:
    \* Lets go: every waker in the set is woken and the slots freed.
    woken := (woken \cup {slot[i] : i \in Ss}) \ {0, self};
    slot := [i \in Ss |-> 0];
    holder := 0;
    done[self] := TRUE;
    goto R_try;
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

\* One holder at a time is the word's (Side.tla); here: every task takes the side in the end.
AllTook == <>(\A r \in Rs : done[r])

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A registration displaces another task's waker.
NoDisplace == ~displaced
=============================================================================

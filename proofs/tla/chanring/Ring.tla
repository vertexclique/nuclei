-------------------------------- MODULE Ring --------------------------------
(***************************************************************************)
(* A bounded channel's ring (src/sync/mpsc/queue/ring.rs): cells used lap  *)
(* after lap, pushed to by producers the channel's semaphore admitted and  *)
(* taken from by one receiver, at the grain of each access.                *)
(*                                                                          *)
(* In plain words: a producer takes a permit (the semaphore, a count of    *)
(* free slots here), claims the next position with a fetch-add, writes its *)
(* value into cell `position % Cells` and stamps the cell with             *)
(* `position + 1`. The receiver takes the cell of its position once its    *)
(* stamp is the position's, reads the value, moves on, and only then gives *)
(* the permit back. So fewer than Cap positions are claimed and not taken, *)
(* and with Cells >= Cap a producer never writes over a value not taken.   *)
(*                                                                          *)
(* What is abstract: a value is the position it was sent for (so reading   *)
(* the wrong lap's value shows); a segment of the ring is made once, which *)
(* changes no answer here and is not modelled.                             *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    Producers,  \* the pushers
    Pushes,     \* how many values each pushes
    Cap,        \* the channel's capacity: the semaphore's permits
    Cells,      \* the ring's cells
    Mutation    \* "none", or the name of the one rule this run takes away

Total == Cardinality(Producers) * Pushes
Positions == 0 .. Total - 1
CellOf(p) == p % Cells
ReceiverT == "receiver"
Null == -1

(* --algorithm Ring
variables
    permits = Cap,                       \* the semaphore's free permits
    tail = 0,                            \* the next position a push claims
    head = 0,                            \* the receiver's next position
    stamp = [c \in 0 .. Cells - 1 |-> 0],
    val = [c \in 0 .. Cells - 1 |-> Null], \* the value in the cell: the position it was sent for
    taken = [p \in Positions |-> 0],
    clobbered = FALSE,                   \* (record) a write fell on a value not taken
    wrongValue = FALSE,                  \* (record) the receiver took another position's value
    lapped = FALSE,                      \* (record) a cell was written a second time
    busySeen = FALSE;                    \* (record) the receiver found its position claimed, not stamped

define
    Done == head = Total
end define;

fair process push \in Producers
variables n = 0, pos = 0;
begin
P_loop:
    while n < Pushes do
P_acquire:
        \* MUTATION no_admit: the push goes in without a permit.
        if Mutation # "no_admit" then
            await permits > 0;
            permits := permits - 1;
        end if;
P_claim:
        pos := tail;
        tail := tail + 1;
P_write:
        \* The cell's earlier value must be one the receiver took.
        if val[CellOf(pos)] # Null /\ taken[val[CellOf(pos)]] = 0 then
            clobbered := TRUE;
        end if;
        lapped := lapped \/ val[CellOf(pos)] # Null;
        val[CellOf(pos)] := pos;
P_stamp:
        stamp[CellOf(pos)] := pos + 1;
        n := n + 1;
    end while;
end process;

fair process receiver = ReceiverT
variables got = Null;
begin
R_loop:
    while ~Done do
R_look:
        \* MUTATION bool_ready: the mark is "written at all", not this lap's stamp.
        if (Mutation = "bool_ready" /\ stamp[CellOf(head)] # 0)
           \/ (Mutation # "bool_ready" /\ stamp[CellOf(head)] = head + 1) then
            \* MUTATION release_before_read: the permit goes back before the read.
            if Mutation = "release_before_read" then
                permits := permits + 1;
                goto R_read;
            else
                got := val[CellOf(head)];
                goto R_took;
            end if;
        else
            busySeen := busySeen \/ head < tail;
            goto R_loop;
        end if;
R_read:
        \* (mutation release_before_read only) The read, after the release.
        got := val[CellOf(head)];
R_took:
        if got # head then
            wrongValue := TRUE;
        end if;
        taken[head] := taken[head] + 1;
        head := head + 1;
R_release:
        if Mutation # "release_before_read" then
            permits := permits + 1;
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

\* A write never falls on a value the receiver has not taken.
NoClobber == ~clobbered

\* The receiver takes the value written for its position, never another lap's.
RightValue == ~wrongValue

\* Every value is taken at most once.
TakenOnce == \A p \in Positions : taken[p] <= 1

\* Positions claimed and not taken never exceed the capacity.
WithinCap == tail - head <= Cap

\* Every value is taken.
AllTaken == <>Done

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A cell is written a second time, a lap later.
NoLap == ~lapped
\* The receiver finds its position claimed and not stamped yet.
NoBusy == ~busySeen
=============================================================================

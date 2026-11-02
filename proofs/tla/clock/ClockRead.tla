------------------------------- MODULE ClockRead -------------------------------
(***************************************************************************)
(* Reading a runtime's clock across a pause (src/time/clock.rs:            *)
(* `Clock::now`, `Clock::now_nanos`, `Clock::pause`, `Clock::resume`;      *)
(* src/time/instant.rs: `Instant::now`), at the grain of each load and     *)
(* store.                                                                  *)
(*                                                                          *)
(* In plain words: a clock that was never paused or advanced reads the     *)
(* system's clock as it is (one load of its `altered` flag, then the       *)
(* system's time), and keeps no record of what it gave. Once altered, a    *)
(* read takes the paused base (or the system's time plus a skew), raises   *)
(* the high-water mark `last` to it, and gives the larger of the two, so   *)
(* reads through that path never go backwards. A pause reads the time,     *)
(* marks the clock altered, stores the base and the paused flag, then      *)
(* raises the base to `last` (a read between its stores may be later) and  *)
(* `last` to the base.                                                     *)
(*                                                                          *)
(* Who acts: the system's clock, which ticks; a thread that pauses the    *)
(* clock and resumes it; and two readers, each reading twice. What must    *)
(* never happen: one reader seeing the clock go backwards.                 *)
(*                                                                          *)
(* What is abstract: time is whole units and the pause's round-up to the   *)
(* millisecond grid is left out (it only moves the base forward).          *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Ticks,     \* how far the system's clock moves within the run
    Mutation   \* "none", or the name of the one rule this run takes away

Readers == {"r1", "r2"}

(* --algorithm ClockRead
variables
    sys = 0,                 \* the system's monotonic clock
    altered = FALSE,         \* the clock was paused or advanced
    paused = FALSE,          \* it stands still
    base = 0,                \* paused: its time
    skew = 0,                \* running after a resume: what is added to the system's time
    last = 0,                \* the high-water mark of the reads through the altered path
    seen = [r \in Readers |-> 0],      \* each reader's last reading
    backwards = FALSE;       \* (record) a reader saw the clock go back

define
    \* No reader sees the clock go backwards.
    ReadsMonotonic == ~backwards
end define;

fair process tick = "tick"
begin
K_tick:
    while sys < Ticks do
        sys := sys + 1;
    end while;
end process;

fair process pauser = "pauser"
variables t = 0;
begin
P_now:
    \* `pause`: the time now (the system's, the clock not altered yet).
    t := sys;
P_alter:
    altered := TRUE;
P_store:
    base := t;
    paused := TRUE;
P_fix:
    \* A read between the stores may be later: the base rises to `last`, `last` to it.
    base := IF last > base THEN last ELSE base;
P_last:
    last := IF base > last THEN base ELSE last;
P_resume:
    \* `resume`: the skew that makes the system's time read the base now.
    skew := base - sys;
    paused := FALSE;
end process;

fair process reader \in Readers
variables k = 0, a = FALSE, v = 0;
begin
R_flag:
    while k < 2 do
        \* `Clock::now`: the altered flag, one load.
        a := altered;
R_read:
        if ~a then
            \* Not altered: the system's time as it is, recorded nowhere.
            v := sys;
        else
            \* `now_nanos`: the base or the system's time plus the skew, raised into `last`.
            with read = IF paused THEN base ELSE sys + skew do
                v := IF read > last THEN read ELSE last;
                last := IF read > last THEN read ELSE last;
            end with;
        end if;
R_check:
        backwards := backwards \/ v < seen[self];
        seen[self] := v;
        k := k + 1;
    end while;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

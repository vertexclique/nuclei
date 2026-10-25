------------------------------ MODULE KillReap ------------------------------
(***************************************************************************)
(* A kill by pid and a reap by pid of one child (src/process/exit.rs:     *)
(* `kill`, `busy`, `reap_by_pid`: kqueue's driver and the SIGCHLD reaper),  *)
(* at the grain of each atomic step.                                       *)
(*                                                                          *)
(* In plain words: a pid is the child's until the child is reaped, and may *)
(* name another process after, so a kill must never come after the reap.  *)
(* Both take the child's state from alive to busy first (waiting while the *)
(* other holds it): the killer signals and gives the state back; the      *)
(* reaper looks whether the child exited, reaps it if so (the state then  *)
(* says reaped), else gives the state back. A killer that finds the child *)
(* reaped does nothing.                                                    *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Mutation   \* "none", or the name of the one rule this run takes away

(* --algorithm KillReap
variables
    state = "alive",      \* "alive", "busy", "reaped"
    exited = FALSE,       \* the child exited (a zombie until reaped)
    reaped = FALSE,
    killAfterReap = FALSE,\* (record) a signal went by pid after the reap
    killed = FALSE,       \* (record) the kill was sent
    skipped = FALSE;      \* (record) the killer found the child reaped

fair process child = "child"
begin
C_exit:
    exited := TRUE;
end process;

fair process killer = "killer"
begin
K_take:
    \* MUTATION no_busy: the killer only looks that the child is not reaped.
    if Mutation = "no_busy" then
        if state = "reaped" then
            skipped := TRUE;
            goto Done;
        end if;
    else
        await state # "busy";
        if state = "reaped" then
            skipped := TRUE;
            goto Done;
        else
            state := "busy";
        end if;
    end if;
K_kill:
    if reaped then killAfterReap := TRUE; end if;
    killed := TRUE;
    exited := TRUE;
K_give:
    if Mutation # "no_busy" then
        state := "alive";
    end if;
end process;

fair process reaper = "reaper"
begin
R_loop:
    while ~reaped do
        await state = "alive";
        state := "busy";
R_wait:
        if exited then
            reaped := TRUE;
            state := "reaped";
        else
            state := "alive";
        end if;
    end while;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

\* No signal ever goes by pid after the reap.
NoKillAfterReap == ~killAfterReap

\* The child is reaped.
Reaps == <>reaped

\* Witnesses: the kill is sent; a killer finds the child reaped already.
NoKill == ~killed
NoSkip == ~skipped
=============================================================================

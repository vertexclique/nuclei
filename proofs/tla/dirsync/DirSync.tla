------------------------------- MODULE DirSync -------------------------------
(***************************************************************************)
(* A directory's sync on Apple's systems (src/fs/sync_dir.rs:              *)
(* `sync_dir_now`), at the grain of one system call.                       *)
(*                                                                         *)
(* In plain words: `sync_dir` writes a directory's entries through. On     *)
(* Apple's systems the strongest call is `F_FULLFSYNC`, which also flushes *)
(* the drive's cache; a file system may refuse it (it answers `ENOTSUP` or *)
(* `EINVAL`). Then the call tries `F_BARRIERFSYNC`, which orders the       *)
(* writes before it ahead of those after it but leaves the cache, and if   *)
(* that is refused too, plain `fsync`, which only hands the entries to the *)
(* drive. Any other error of a call (an I/O error) is the answer, with no  *)
(* weaker call after it. The rustdoc says which of the three a file        *)
(* system gets, so an `Ok` never claims more than the call made.           *)
(*                                                                         *)
(* What must never happen: an `Ok` after a weaker call than the strongest  *)
(* the file system takes (`Strongest`); an `Ok` though every call was      *)
(* refused (`NoOkWithoutSync`); an I/O error turned into a weaker call's   *)
(* `Ok` (`ErrorStands`). What must happen: the call ends (`Termination`).  *)
(*                                                                         *)
(* What is abstract: a file system is the set of calls it takes, and       *)
(* whether one of them fails with an I/O error; the calls' effects on the  *)
(* device are their names.                                                 *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Takes,     \* the calls the file system takes, a subset of Kinds
    Fails,     \* the call that fails with an I/O error, or "none"
    Mutation   \* "none", or the name of the one rule this run takes away

Kinds == <<"full", "barrier", "plain">>   \* strongest first
Rank(k) == CHOOSE i \in 1..3 : Kinds[i] = k   \* a call's place, 1 the strongest
Strongest(s) == CHOOSE k \in s : \A j \in s : Rank(k) <= Rank(j)   \* the strongest call of a set

(* --algorithm DirSync
variables
    made = "none",        \* the call that answered first without a refusal
    answer = "none",      \* the sync's answer: "none" until it ends, "ok", "refused" or "error"
    tried = 0;            \* how many calls were made

define
    \* The sync has ended.
    Ended == pc[1] = "Done"
    \* An `Ok` comes from the strongest call the file system takes.
    StrongestMade == (Ended /\ answer = "ok") => made = Strongest(Takes)
    \* Every call refused: the answer is the refusal, never `Ok`.
    NoOkWithoutSync == (Ended /\ Takes = {}) => answer # "ok"
    \* A call that failed with an I/O error is the answer.
    ErrorStands == (Ended /\ Fails \in Takes /\ Fails = Strongest(Takes)) => answer = "error"
    \* Witness: a sync can answer `Ok` after a weaker call than `F_FULLFSYNC`.
    NoWeakOk == (Ended /\ answer = "ok") => made = "full"
end define;

fair process sync = 1                       \* the blocking pool's job of `sync_dir`
variables i = 1;                            \* the call to make next, 1 the strongest
begin
S_call:
    while i <= 3 do                         \* the calls, strongest first
        if Mutation = "skip_barrier" /\ Kinds[i] = "barrier" then   \* the broken chain: no barrier
            i := i + 1;                     \* it goes on to `fsync`
        elsif Kinds[i] \in Takes then       \* the file system takes this call
            tried := tried + 1;             \* it is made
            if Fails = Kinds[i] then        \* and fails with an I/O error
                if Mutation = "fall_on_error" then   \* the broken chain: any error falls back
                    i := i + 1;             \* to a weaker call
                else
                    answer := "error";      \* the error is the answer
                    goto Done;
                end if;
            else
                made := Kinds[i];           \* the call went through
                answer := "ok";             \* and the sync says so
                goto Done;
            end if;
        else                                \* refused (`ENOTSUP` or `EINVAL`)
            tried := tried + 1;             \* it was made
            i := i + 1;                     \* the next, weaker, call
        end if;
    end while;
S_refused:
    if Mutation = "ok_on_refusal" then      \* the broken end: every refusal answered as done
        answer := "ok";
    else
        answer := "refused";                \* the last refusal is the answer
    end if;
end process;
end algorithm; *)
=============================================================================

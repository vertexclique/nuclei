------------------------------ MODULE PortFile ------------------------------
(***************************************************************************)
(* Reads and writes of a Windows disk file made by threads of the blocking *)
(* pool (src/io/sys/windows.rs `read_at`, `write_all_at`, and `at` and     *)
(* `finished`, the `OVERLAPPED` each call names and its end), on a handle  *)
(* opened for overlapped IO (`FILE_FLAG_OVERLAPPED`, as a database opens   *)
(* its log with `FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH`) or for *)
(* synchronous IO.                                                         *)
(*                                                                          *)
(* Who acts. Two threads each make one call on the same handle: a write at *)
(* the cursor and a write at an offset of its own. Each call names an      *)
(* `OVERLAPPED` in its own frame. The kernel takes each call, and ends it  *)
(* at some time, writing its status into that `OVERLAPPED` and signalling  *)
(* the call's event, or, for a call that names none, the handle itself.    *)
(*                                                                          *)
(* What can happen. On a synchronous handle the call returns once the      *)
(* kernel ended it. On an overlapped handle the call may come back at once *)
(* with ERROR_IO_PENDING, the kernel still holding it: the call then waits *)
(* on its own event (the calling thread's, `at`) for its own end           *)
(* (`GetOverlappedResult` with its wait), and returns what the kernel said *)
(* of it.                                                                  *)
(*                                                                          *)
(* What must never happen. The kernel writes a call's status into a frame  *)
(* the call has left (a stack the thread has gone on with: memory          *)
(* corruption, a read's bytes landing in a buffer handed back). A call     *)
(* answers anything but what the kernel said of it (ERROR_IO_PENDING taken *)
(* for a failure, though the write lands). A call never returns.           *)
(*                                                                          *)
(* What is abstract. A call is its number; the bytes and the offsets; the  *)
(* event's reset (each call clears it as it starts, `ReadFile` and         *)
(* `WriteFile`'s own rule).                                                *)
(***************************************************************************)
EXTENDS Integers, TLC                                                        \* numbers and TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Overlapped,  \* TRUE: the handle was opened for overlapped IO; FALSE: for synchronous IO
    Mutation     \* "none", or the name of the one rule this run takes away

Calls == {1, 2}                                                              \* the cursor write and the positional one

(* --algorithm PortFile                                                        \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    held = {},                     \* the calls the kernel holds
    ended = {},                    \* the calls the kernel ended (status written, event signalled)
    handleSignal = FALSE,          \* the handle's own signal: set as any call on it ends
    left = {},                     \* the calls whose frames are gone (the call returned)
    result = [c \in Calls |-> "none"], \* what each call answered
    deadWrite = FALSE;             \* (record) the kernel wrote into a frame its call left

\* The kernel: it ends each call it holds, at some time, writing into the call's frame.
fair process kernel = "kernel"                                               \* weakly fair: what it holds ends
begin                                                                        \* the kernel's steps:
K_end:                                                                       \* one call's end
    while TRUE do                                                            \* for as long as the run goes
        await held # {};                                                     \* a call held
        with c \in held do                                                   \* whichever ends first
            held := held \ {c};                                              \* no longer held
            ended := ended \union {c};                                       \* its status and its event
            handleSignal := TRUE;                                            \* and the handle's signal
            deadWrite := deadWrite \/ c \in left;                            \* (record) into a gone frame
        end with;                                                            \* one ended
    end while;                                                               \* the next
end process;                                                                 \* end of the kernel

\* A pool thread's call.
fair process call \in Calls                                                  \* weakly fair, like every process
begin                                                                        \* the call's steps:
C_issue:                                                                     \* `WriteFile` with its `OVERLAPPED`
    held := held \union {self};                                              \* the kernel takes it
    if Overlapped then                                                       \* it may come back at once, pending:
        either                                                               \* the kernel ended it already
            await self \in ended;                                            \* (it completed in the call)
            result[self] := "ok";                                            \* its own answer
            goto C_return;                                                   \* the call returns
        or                                                                   \* ERROR_IO_PENDING
            skip;                                                            \* the kernel still holds it
        end either;                                                          \* the call came back
    else                                                                     \* a synchronous handle:
        await self \in ended;                                                \* the call returns once it ended
        result[self] := "ok";                                                \* its own answer
        goto C_return;                                                       \* the call returns
    end if;                                                                  \* still pending
C_pending:                                                                   \* `finished`
    \* MUTATION no_wait: the call answers ERROR_IO_PENDING as its error and returns, as it did.
    if Mutation = "no_wait" then                                             \* (mutated) at once:
        result[self] := "err";                                               \* a false error
        goto C_return;                                                       \* and the frame is gone
    end if;                                                                  \* else it waits:
C_wait:                                                                      \* `GetOverlappedResult`, waiting
    \* MUTATION handle_wait: it waits on the handle's signal, which any call's end sets, instead of its own event.
    if Mutation = "handle_wait" then                                         \* (mutated) the handle:
        await handleSignal;                                                  \* set by whichever ended
        handleSignal := FALSE;                                               \* taken
    else                                                                     \* its own event:
        await self \in ended;                                                \* set by its own end only
    end if;                                                                  \* woken
    result[self] := IF self \in ended THEN "ok" ELSE "incomplete";           \* what it read of its status
C_return:                                                                    \* the call returns
    left := left \union {self};                                              \* its frame is gone
end process;                                                                 \* end of a call
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* The kernel never writes a call's status into a frame the call has left.
NoDeadWrite == ~deadWrite                                                    \* never so

\* Each call answers what the kernel said of it: every write here lands.
OwnResult == \A c \in Calls : result[c] \in {"none", "ok"}                   \* never an error it did not have

\* Every call returns.
Returns == <>(left = Calls)                                                  \* both returned

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A call came back pending and waited for its own end.
NoPendingWait == ~(\E c \in Calls : pc[c] = "C_wait")                        \* never so
=============================================================================

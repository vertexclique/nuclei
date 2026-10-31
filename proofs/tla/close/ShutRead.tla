------------------------------- MODULE ShutRead -------------------------------
(***************************************************************************)
(* A stream's reading half shut (src/net/close.rs: `Handle::shut` with     *)
(* `Shutdown::Read` or `Shutdown::Both`; src/io/source/read/calls.rs:      *)
(* `Source::poll_drop_ahead`, which calls `ReadState::drop_ahead` once the *)
(* kernel shut the half; src/fs/ring.rs: the shutdown entry a ring makes). *)
(*                                                                          *)
(* Who acts. The peer sent bytes the task has not read yet: some wait in   *)
(* the handle's read-ahead (the driver took them from the kernel for an    *)
(* earlier read), the rest in the kernel's receive queue. The task shuts   *)
(* the reading half, then reads until a read answers the end. The driver   *)
(* moves bytes from the kernel's queue into the read-ahead whenever it     *)
(* runs, the shutdown or not.                                              *)
(*                                                                          *)
(* What can happen. Linux keeps what its receive queue holds readable      *)
(* after the shutdown, then answers the end. macOS (and the other BSDs)    *)
(* and Windows drop the queue as they shut the half: there the handle      *)
(* drops what it read ahead too, once the kernel shut the half, so a read  *)
(* after the shut answers as the kernel does.                              *)
(*                                                                          *)
(* What must never happen. A read after the shut returned answers what     *)
(* std's would not on the same stream, std having left every one of those  *)
(* bytes in the kernel: on Linux every byte and then the end, elsewhere    *)
(* the end at once.                                                        *)
(*                                                                          *)
(* What is abstract. The bytes are counted; Windows answers a read after   *)
(* the shutdown with an error where the others answer the end, which is    *)
(* the kernel's own answer to both (`eof` stands for either); a receive a  *)
(* completion port already filled before the shutdown, its completion not  *)
(* yet taken, is not modelled: its bytes come after the drop.              *)
(***************************************************************************)
EXTENDS Integers, TLC                                                        \* numbers and TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Sent,      \* the bytes the peer sent that the task has not read
    Ahead,     \* of them, the bytes the handle read ahead before the shut
    Platform,  \* "linux", or "other": a kernel that drops its queue as it shuts the half
    Mutation   \* "none", or the one rule this run takes away

\* What std reads after the same shutdown with all `Sent` bytes still in its kernel.
StdReads == IF Platform = "linux" THEN Sent ELSE 0                           \* Linux all of them, the others none

\* Whether the handle drops its read-ahead after the kernel's shutdown.
\* MUTATION keep_ahead: it never does. MUTATION drop_on_linux: it does on Linux too.
\* MUTATION drop_first: it drops it before the kernel's shutdown instead, then never again.
Drops == IF Platform = "linux" THEN Mutation = "drop_on_linux"               \* Linux keeps its queue
         ELSE Mutation \notin {"keep_ahead", "drop_first"}                   \* the others drop theirs

(* --algorithm ShutRead                                                       \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    queue = Sent - Ahead,  \* bytes in the kernel's receive queue
    ahead = Ahead,         \* bytes the handle read ahead, not read yet
    shut = FALSE,          \* the kernel shut the reading half
    gone = 0,              \* (record) bytes the handle's drop of its read-ahead dropped
    returned = FALSE,      \* the task's shut returned
    readAfter = 0,         \* bytes the task read after its shut returned
    eof = FALSE;           \* a read after the shut answered the end

\* The task: shuts the reading half, then reads until the end.
fair process task = "task"                                                   \* one task
begin                                                                        \* the task's steps:
T_early:                                                                     \* the mutation's early drop
    if Mutation = "drop_first" /\ Platform # "linux" then                    \* (only the mutation):
        gone := gone + ahead;                                                \* it drops the read-ahead now
        ahead := 0;                                                          \* before the kernel shuts the half
    end if;                                                                  \* done
T_shut:                                                                      \* the kernel's shutdown
    shut := TRUE;                                                            \* the reading half is shut
    if Platform # "linux" then                                               \* a kernel that drops its queue:
        queue := 0;                                                          \* drops it now
    end if;                                                                  \* shut
T_drop:                                                                      \* the read side's drop
    if Drops then                                                            \* where the kernel dropped its queue:
        gone := gone + ahead;                                                \* (record) what it held
        ahead := 0;                                                          \* the read-ahead goes too
    end if;                                                                  \* dropped
    returned := TRUE;                                                        \* the shut returns
T_read:                                                                      \* reads until the end
    while ~eof do                                                            \* until a read answers the end
        either                                                               \* a byte read ahead:
            await ahead > 0;                                                 \* the handle holds one
            ahead := ahead - 1;                                              \* the read takes it
            readAfter := readAfter + 1;                                      \* after the shut returned
        or                                                                   \* the end:
            await ahead = 0 /\ queue = 0;                                    \* nothing ahead nor queued
            eof := TRUE;                                                     \* the shut half answers the end
        end either;                                                          \* one of the two
    end while;                                                               \* the end came
end process;                                                                 \* end of the task

\* The driver: reads ahead from the kernel's queue whenever it runs.
fair process driver = "driver"                                               \* the shard's driver
begin                                                                        \* the driver's steps:
D_loop:                                                                      \* one byte per step
    while ~eof do                                                            \* until the task saw the end
        await queue > 0 \/ eof;                                              \* a byte queued, or the end came
        if queue > 0 then                                                    \* a byte:
            queue := queue - 1;                                              \* it leaves the kernel's queue
            ahead := ahead + 1;                                              \* into the read-ahead
        end if;                                                              \* moved
    end while;                                                               \* done
end process;                                                                 \* end of the driver
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* Once a read after the shut answered the end, the task read exactly what std reads after its shutdown.
AsStd == eof => readAfter = StdReads                                         \* the kernel's answer, not the handle's

\* `Termination`, which the translation defines: the task reads to the end, and the driver stops.

(***************************************************************************)
(* Witness: claims an interesting case never happens and must be proved    *)
(* wrong.                                                                  *)
(***************************************************************************)

\* The drop of the read-ahead drops bytes.
NothingDropped == gone = 0                                                   \* broken where the kernel drops its queue
=============================================================================

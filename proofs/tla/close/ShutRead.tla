------------------------------- MODULE ShutRead -------------------------------
(***************************************************************************)
(* A stream's reading half shut (src/net/close.rs: `Handle::shut` with     *)
(* `Shutdown::Read` or `Shutdown::Both`; src/io/source/read/calls.rs:      *)
(* `Source::poll_drop_ahead`, which calls `ReadState::drop_ahead` once the *)
(* kernel shut the half and marks the side shut; src/io/source/read.rs:    *)
(* `take_stream`, which answers the end for good once the side is shut,    *)
(* and the completions of src/io/source/read/ops.rs, which drop the bytes  *)
(* a receive brings into a side that is shut; src/fs/ring.rs: the shutdown *)
(* entry a ring makes).                                                    *)
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
(* drops what it read ahead too, once the kernel shut the half, and marks  *)
(* the side shut: a read after the shut answers the end, with no call, and *)
(* bytes a receive brings later are dropped as they come. Windows answers  *)
(* a read made after the shutdown with an error (`WSAESHUTDOWN`) where the *)
(* others answer the end: the mark keeps that read from the kernel, so it  *)
(* answers the end there too (`Platform = "windows"`).                     *)
(*                                                                          *)
(* What must never happen. A read after the shut returned answers what     *)
(* std's would not on the same stream, std having left every one of those  *)
(* bytes in the kernel: on Linux every byte and then the end, elsewhere    *)
(* the end at once.                                                        *)
(*                                                                          *)
(* What is abstract. The bytes are counted.                                *)
(*                                                                          *)
(* With `PortFilled` set, a completion port's receive (Windows) takes      *)
(* bytes from the kernel's queue into a buffer of the pool before the      *)
(* shutdown, and its completion reaches the side only after the drop of    *)
(* the read-ahead. The side is shut by then, so the completion's bytes are *)
(* dropped, and a read after the shut answers the end as std's does. Before *)
(* (`late_kept`), they became read-ahead after the shut, and a read        *)
(* returned them.                                                          *)
(***************************************************************************)
EXTENDS Integers, TLC                                                        \* numbers and TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Sent,      \* the bytes the peer sent that the task has not read
    Ahead,     \* of them, the bytes the handle read ahead before the shut
    Platform,  \* "linux"; "other", a kernel that drops its queue as it shuts the half; "windows", one that
               \* also answers a read made after the shutdown with an error
    PortFilled, \* TRUE: a completion port's receive took bytes before the shutdown, its completion not taken yet
    Mutation   \* "none", or the one rule this run takes away

\* What std reads after the same shutdown with all `Sent` bytes still in its kernel.
StdReads == IF Platform = "linux" THEN Sent ELSE 0                           \* Linux all of them, the others none

\* Whether the handle drops its read-ahead after the kernel's shutdown.
\* MUTATION keep_ahead: it never does. MUTATION drop_on_linux: it does on Linux too.
\* MUTATION drop_first: it drops it before the kernel's shutdown instead, then never again.
Drops == IF Platform = "linux" THEN Mutation = "drop_on_linux"               \* Linux keeps its queue
         ELSE Mutation \notin {"keep_ahead", "drop_first"}                   \* the others drop theirs
\* Whether the side is marked shut as it drops its read-ahead: wherever it drops it.
\* MUTATION kernel_answers: it is never marked, so a read after the shut asks the kernel.
Marks == Drops /\ Mutation # "kernel_answers"

(* --algorithm ShutRead                                                       \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    queue = Sent - Ahead,  \* bytes in the kernel's receive queue
    ahead = Ahead,         \* bytes the handle read ahead, not read yet
    shut = FALSE,          \* the kernel shut the reading half
    gone = 0,              \* (record) bytes the handle's drop of its read-ahead dropped
    returned = FALSE,      \* the task's shut returned
    readAfter = 0,         \* bytes the task read after its shut returned
    eof = FALSE,           \* a read after the shut answered the end
    errored = FALSE,       \* a read after the shut answered the kernel's error
    marked = FALSE,        \* the side is marked shut: reads answer the end, late bytes are dropped
    filled = 0;            \* bytes a port's receive took from the queue, its completion not taken yet

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
        marked := Marks;                                                     \* and the side is marked shut
    end if;                                                                  \* dropped
    returned := TRUE;                                                        \* the shut returns
T_read:                                                                      \* reads until the end
    while ~eof do                                                            \* until a read answers the end
        either                                                               \* a byte read ahead:
            await ahead > 0;                                                 \* the handle holds one (read first, as `take_stream` does)
            ahead := ahead - 1;                                              \* the read takes it
            readAfter := readAfter + 1;                                      \* after the shut returned
        or                                                                   \* the side marked shut:
            await marked /\ ahead = 0;                                       \* the handle answers the end itself
            eof := TRUE;                                                     \* with no call
        or                                                                   \* the kernel's answer:
            await ~marked /\ ahead = 0 /\ queue = 0 /\ filled = 0;            \* nothing ahead, queued nor coming
            eof := TRUE;                                                     \* the read ends:
            errored := (Platform = "windows");                               \* in the end, or Windows' error
        end either;                                                          \* one of the three
    end while;                                                               \* the end came
end process;                                                                 \* end of the task

\* The driver: reads ahead from the kernel's queue whenever it runs.
fair process driver = "driver"                                               \* the shard's driver
begin                                                                        \* the driver's steps:
D_loop:                                                                      \* one byte per step
    while ~eof do                                                            \* until the task saw the end
        await queue > 0 \/ eof \/ (filled > 0 /\ returned);                  \* a byte queued, a late completion, or the end
        if filled > 0 /\ returned then                                       \* the port's completion, taken after the drop:
            \* MUTATION late_kept: its bytes become read-ahead whatever the mark says (the code before).
            if marked /\ Mutation # "late_kept" then                         \* the side is shut:
                gone := gone + filled;                                       \* (record) its bytes are dropped
            else                                                             \* else
                ahead := ahead + filled;                                     \* its bytes are read-ahead now
            end if;
            filled := 0;
        elsif PortFilled /\ ~shut /\ queue > 0 then                          \* a port's receive before the shutdown:
            filled := filled + queue;                                        \* takes what is queued, its completion later
            queue := 0;
        elsif queue > 0 then                                                 \* a byte:
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

\* A read after the shut answers the end, never an error: as on unix, on Windows too.
AnswersEnd == ~errored                                                       \* no kernel error reached the task

\* `Termination`, which the translation defines: the task reads to the end, and the driver stops.

(***************************************************************************)
(* Witness: claims an interesting case never happens and must be proved    *)
(* wrong.                                                                  *)
(***************************************************************************)

\* The drop of the read-ahead drops bytes.
NothingDropped == gone = 0                                                   \* broken where the kernel drops its queue
=============================================================================

-------------------------------- MODULE Close --------------------------------
(***************************************************************************)
(* A TCP stream's ends: `shut` and `abort` (src/net/close.rs:              *)
(* `Handle::shut`, `Handle::abort`; src/io/source/write/calls.rs:          *)
(* `Source::poll_shutdown`; src/io/source/write.rs: `WriteState::close`,   *)
(* which flushes and then calls `sys::shutdown`, and `WriteState::push_fd`, *)
(* the driver's writes), and the drop that follows them (src/io/handle.rs: *)
(* the handle's drop, `Handle::close_now`, `linger`;                       *)
(* src/io/source/requests.rs: `Source::close`).                            *)
(*                                                                          *)
(* Who acts. A task writes bytes to the stream: they wait in the           *)
(* write-behind until the driver hands them to the kernel, whose send      *)
(* queue holds a few until the peer takes them. Then the task ends the     *)
(* stream one of five ways: it shuts the writing half, or both halves, or  *)
(* the reading half, and drops the handle; it aborts; or it drops the      *)
(* handle at once. A handle dropped with bytes still behind hands them to  *)
(* a linger task, which drains them for at most the runtime's linger time  *)
(* (`Builder::write_behind_linger`) and then closes. A close is performed  *)
(* by the driver, the side's holder as it lets go. The peer reads what     *)
(* comes, or never reads.                                                  *)
(*                                                                          *)
(* What can happen. A shut of the writing half, or of both, waits until    *)
(* the write-behind is out, then asks the kernel to shut it, and the       *)
(* kernel sends the end of the stream (a FIN) after its queue. A shut of   *)
(* the reading half leaves the writes alone. An abort sets SO_LINGER to 0, *)
(* drops the write-behind and asks for the close with no linger: the close *)
(* resets the connection (an RST) and the kernel drops its queue. A linger *)
(* whose time runs out with bytes still behind gives them up and resets    *)
(* the connection too.                                                     *)
(*                                                                          *)
(* What must never happen. The peer gets a FIN while a byte the task wrote *)
(* never reached it: it would take a cut stream for a whole one. An abort  *)
(* drains the bytes behind, or ends with a FIN.                            *)
(*                                                                          *)
(* What is abstract. The bytes are counted, and go out in order; the       *)
(* kernel's queue holds `Room` of them; the peer reads one at a time, or   *)
(* never; the linger's time running out is a step that may come at any    *)
(* moment while bytes are behind; the reading half's own effects are       *)
(* ShutRead.tla's.                                                         *)
(***************************************************************************)
EXTENDS Integers, TLC                                                        \* numbers and TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Bytes,     \* the bytes the task writes before it ends the stream
    Room,      \* the bytes the kernel's send queue holds
    Ends,      \* the ends the task may choose: a set of "shut_write", "shut_both", "shut_read", "abort", "drop"
    Peer,      \* "reads": the peer takes what comes; "stalls": it never reads
    Mutation   \* "none", or the one rule this run takes away

(* --algorithm Close                                                          \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    written = 0,        \* bytes the task wrote, each taken into the write-behind
    behind = 0,         \* bytes in the write-behind that the driver has not handed to the kernel
    queued = 0,         \* bytes in the kernel's send queue that the peer has not taken
    got = 0,            \* bytes the peer took: the first `got` the task wrote, in order
    lost = 0,           \* bytes that will never reach the peer
    fin = FALSE,        \* the kernel was asked to end the stream after its queue
    zero = FALSE,       \* SO_LINGER is 0, so a close resets
    shutWr = FALSE,     \* the kernel's writing half is shut
    closeAsked = FALSE, \* the descriptor's close was asked of the source
    closed = FALSE,     \* the descriptor is closed
    ended = "none",     \* how the peer saw the stream end: "none" yet, "fin" or "rst"
    lingering = FALSE,  \* the dropped handle's bytes were handed to a linger task
    expired = FALSE,    \* (record) the linger's time ran out with bytes behind
    aborted = FALSE,    \* the task aborted
    waited = FALSE;     \* (record) a shut found bytes behind, which it waits for

\* The task: writes its bytes, then ends the stream the way it chooses.
fair process task = "task"                                                   \* one task
variables how = "none";                                                      \* the end it chose
begin                                                                        \* the task's steps:
T_write:                                                                     \* one byte per step
    while written < Bytes do                                                 \* until every byte is written
        written := written + 1;                                              \* the write is accepted
        behind := behind + 1;                                                \* its byte waits in the write-behind
    end while;                                                               \* every byte written
T_choose:                                                                    \* the end of the stream
    with e \in Ends do                                                       \* any end this run allows
        how := e;                                                            \* is the one it takes
    end with;                                                                \* chosen
    if how = "shut_write" \/ how = "shut_both" then                          \* a shut of the writing half (or of both):
        waited := behind > 0;                                                \* (record) bytes are still behind
T_drain:                                                                     \* the flush
        \* It waits until the driver handed every byte to the kernel.
        \* MUTATION shut_first: a shut of the writing half does not wait.
        \* MUTATION both_first: a shut of both halves does not wait.
        await \/ behind = 0                                                  \* the write-behind is out
              \/ how = "shut_write" /\ Mutation = "shut_first"               \* (the mutation's)
              \/ how = "shut_both" /\ Mutation = "both_first";               \* (the mutation's)
T_shut:                                                                      \* then the kernel's shutdown
        shutWr := TRUE;                                                      \* the writing half is shut
        fin := TRUE;                                                         \* the FIN goes after the kernel's queue
    elsif how = "abort" then                                                 \* an abort:
        \* SO_LINGER 0, so the close resets.
        \* MUTATION abort_fin: the abort closes without setting it.
        zero := Mutation # "abort_fin";                                      \* set first, as `abort` does
        aborted := TRUE;                                                     \* the task aborted
        \* MUTATION abort_lingers: the abort drops the handle as a drop does, whose linger drains first.
        if Mutation # "abort_lingers" then                                   \* `close_now`, not the drop:
            lost := lost + behind;                                           \* the write-behind is dropped
            behind := 0;                                                     \* with what it held
            closeAsked := TRUE;                                              \* and the close is asked at once
            goto T_done;                                                     \* nothing lingers
        end if;                                                              \* (the mutation goes on to the drop)
    end if;                                                                  \* a shut of the reading half, or a drop, goes on
T_drop:                                                                      \* the handle's drop
    if behind > 0 then                                                       \* bytes are still behind:
        lingering := TRUE;                                                   \* a linger task drains them
    else                                                                     \* none:
        closeAsked := TRUE;                                                  \* the close is asked at once
    end if;                                                                  \* the handle is gone
T_done:                                                                      \* the task is done
    skip;                                                                    \* nothing more
end process;                                                                 \* end of the task

\* The driver: hands the write-behind to the kernel while it has room, and performs the close.
fair process driver = "driver"                                               \* the shard's driver
begin                                                                        \* the driver's steps:
D_loop:                                                                      \* one call per step
    while ~closed do                                                         \* until the descriptor is closed
        either                                                               \* a write of the oldest byte behind:
            await behind > 0 /\ ~shutWr /\ queued < Room;                    \* while the half is open and has room
            behind := behind - 1;                                            \* it leaves the write-behind
            queued := queued + 1;                                            \* into the kernel's queue
        or                                                                   \* a write into a shut half:
            await behind > 0 /\ shutWr;                                      \* bytes behind it
            lost := lost + behind;                                           \* the kernel refuses them (EPIPE)
            behind := 0;                                                     \* and the write-behind fails
        or                                                                   \* the close:
            await closeAsked;                                                \* asked of the source
            closed := TRUE;                                                  \* the descriptor closes
            if zero then                                                     \* with SO_LINGER 0:
                lost := lost + queued + behind;                              \* the kernel drops its queue
                queued := 0;                                                 \* nothing more goes out
                behind := 0;                                                 \* nor from the write-behind
                if ended = "none" then                                       \* unless the stream ended already
                    ended := "rst";                                          \* the peer gets a reset
                end if;                                                      \* reset
            else                                                             \* a plain close:
                fin := TRUE;                                                 \* the FIN goes after the kernel's queue
                lost := lost + behind;                                       \* bytes still behind never go
                behind := 0;                                                 \* out
            end if;                                                          \* closed
        end either;                                                          \* one of the three
    end while;                                                               \* closed
end process;                                                                 \* end of the driver

\* The linger task: drains a dropped handle's bytes for at most the linger time, then closes.
fair process linger = "linger"                                               \* the task the drop hands
begin                                                                        \* the linger's steps:
L_wait:                                                                      \* it waits to be handed bytes
    await lingering \/ closeAsked;                                           \* a close asked without it hands it none
    if lingering then                                                        \* handed the bytes:
L_drain:                                                                     \* the drain
        either                                                               \* it ends:
            await behind = 0;                                                \* every byte reached the kernel
        or                                                                   \* or it ends first (its time ran out, or it failed):
            await behind > 0;                                                \* bytes still behind
            expired := TRUE;                                                 \* (record) it ran out
            \* The bytes behind are given up, and SO_LINGER 0 tells the peer so.
            \* MUTATION expiry_fin: the linger closes as a plain close, which ends with a FIN.
            if Mutation # "expiry_fin" then                                  \* (not the mutation):
                zero := TRUE;                                                \* the close resets
            end if;                                                          \* set
            lost := lost + behind;                                           \* the bytes behind are given up
            behind := 0;                                                     \* gone
        end either;                                                          \* one of the two
L_close:                                                                     \* then the close
        closeAsked := TRUE;                                                  \* asked of the source
    end if;                                                                  \* done
end process;                                                                 \* end of the linger

\* The peer: takes the bytes of the kernel's queue, and the FIN after them.
fair process wire = "wire"                                                   \* the connection's far end
begin                                                                        \* the peer's steps:
W_loop:                                                                      \* one at a time
    while ended = "none" do                                                  \* until the stream ended
        either                                                               \* a byte:
            await Peer = "reads" /\ queued > 0;                              \* the peer reads, and one is queued
            queued := queued - 1;                                            \* it leaves the kernel's queue
            got := got + 1;                                                  \* the peer has it
        or                                                                   \* the FIN:
            await queued = 0 /\ fin;                                         \* after the last queued byte
            ended := "fin";                                                  \* the stream ends cleanly
        end either;                                                          \* one of the two (an RST ends it too)
    end while;                                                               \* ended
end process;                                                                 \* end of the peer
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* Every variable holds a value of its kind.
TypeOK == /\ written \in 0..Bytes                                            \* a count of the task's bytes
          /\ behind \in 0..Bytes                                             \* so is what is behind
          /\ queued \in 0..Room                                              \* the kernel holds at most its room
          /\ got \in 0..Bytes                                                \* the peer has at most every byte
          /\ lost \in 0..Bytes                                               \* and at most every byte is lost
          /\ ended \in {"none", "fin", "rst"}                                \* one of the three ends

\* No byte is made up or counted twice: each is behind, queued, at the peer, or lost.
Conserved == written = behind + queued + got + lost                          \* the counts add up

\* A FIN reaches the peer only once every byte the task wrote reached it, in order: the peer never takes a
\* cut stream for a whole one.
FinMeansWhole == ended = "fin" => got = written                              \* the end follows every byte

\* An abort's stream ends with a reset, never with a FIN.
AbortResets == aborted /\ ended # "none" => ended = "rst"                    \* the peer is told

\* An abort never hands the bytes behind to a linger task, which would drain them first.
AbortDoesNotDrain == aborted => ~lingering                                   \* it closes now

\* `Termination`, which the translation defines: every process ends, so every end the task chooses is
\* reached, every drain ends, and the peer learns that the stream ended.

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be    *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* The peer gets a reset.
NoReset == ended # "rst"                                                     \* broken by an abort

\* A linger's time runs out with bytes still behind.
NoExpiry == ~expired                                                         \* broken by a peer that never reads

\* A shut finds bytes behind and waits for them.
NoShutWait == ~waited                                                        \* broken by a shut after a full queue
=============================================================================

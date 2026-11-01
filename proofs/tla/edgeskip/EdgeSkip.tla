------------------------------ MODULE EdgeSkip ------------------------------
(***************************************************************************)
(* The epoll driver passing over a write edge no writer waits for, on a   *)
(* machine whose stores wait in a store buffer, as x86's do              *)
(* (src/io/source.rs: `Source::on_event`, `write_wants_no_edge`, and the *)
(* write side's `releasing` hook in src/io/source/hooks.rs, which         *)
(* publishes `write_idle`; src/io/side.rs: `Side::biased_to`, the foreign *)
(* take `Side::take_foreign`; src/io/source/write.rs: `WriteState::idle`, *)
(* the condition `WriteState::serve` only counts an edge under).          *)
(*                                                                          *)
(* Who acts. The owner thread, the one running the source's home shard,   *)
(* drives the epoll events. A foreign writer, a task on another thread    *)
(* that holds a `Send` handle, writes on the same socket. The kernel      *)
(* posts a write edge once room comes after a write it refused. A flusher *)
(* stands for the memory: the oldest store of the writer's buffer lands  *)
(* at any moment.                                                          *)
(*                                                                          *)
(* What can happen. Every holder of the write side publishes, as it lets  *)
(* go, whether the side waits for nothing an edge brings (`write_idle`): *)
(* no write blocked, nothing written behind, no connect, no lent buffer, *)
(* no caller's own calls. The driver, given a write edge, passes over it  *)
(* when the side was only ever held by its own thread (biased to it, not *)
(* shared, and no foreign thread came: `intent` down) and that thread's   *)
(* last release said idle; else it takes the side and                     *)
(* serves the edge, or leaves READY for the holder, as always. A foreign *)
(* writer on a biased side raises `intent` with a locked instruction and  *)
(* runs the heavy fence before it holds the side; on a side made shared  *)
(* (no heavy fence in the process) it takes the word at once. A write the *)
(* socket refuses blocks the side, and the kernel posts an edge once room *)
(* comes, after the refusal.                                               *)
(*                                                                          *)
(* What must never happen. The driver passes over the edge a blocked      *)
(* writer waits for, and the writer waits for good (a deadlock here). A   *)
(* foreign holder's `write_idle` may still sit in its buffer when the     *)
(* edge comes; the mode switch it stored before its locked take of the   *)
(* word is in memory by then, so the look sees the side shared.          *)
(*                                                                          *)
(* What is abstract. The owner thread's own polls run on the driver's    *)
(* thread, between its events, so its releases come before the driver's  *)
(* look in program order: the model starts from its last one, which said *)
(* idle. One foreign write, refused once at most; the write side's state *)
(* is whether it blocks. The kernel's edge is posted after the refusal.  *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC                                             \* numbers, sequences (the buffer), TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Biased,     \* TRUE: the side starts biased to its owner; FALSE: shared (no heavy fence)
    Spurious,   \* TRUE: an edge comes first that no write asked for (epoll reports room with every read)
    Mutation    \* "none", or the name of the one rule this run takes away

Range(s) == {s[k] : k \in DOMAIN s}                                          \* the stores a buffer holds

(* --algorithm EdgeSkip
variables                                                                    \* the shared state:
    intent = FALSE,           \* a foreign thread came, for good (in memory)
    shared = ~Biased,         \* the side takes through its word only (in memory)
    idle = TRUE,              \* the published `write_idle` (in memory): the owner's last release said idle
    bufW = <<>>,              \* the foreign writer's store buffer, oldest first
    word = "free",            \* who holds the side through its word: "free", "W" or "D"
    ready = FALSE,            \* READY left in the word for its holder
    blocked = FALSE,          \* the write side waits for an edge (a write was refused)
    nospace = FALSE,          \* the kernel owes an edge: room comes after the refusal
    edge = FALSE,             \* a write edge waits for the driver
    done = FALSE,             \* the foreign write went out
    skipped = FALSE;          \* (record) the driver passed over an edge

\* A locked instruction of the writer: its buffer lands first.
macro drain() begin
    if "shared" \in Range(bufW) then shared := TRUE; end if;                 \* a buffered mode switch lands
    if "busy" \in Range(bufW) then idle := FALSE; end if;                    \* a buffered `write_idle` lands
    bufW := <<>>;                                                            \* and the buffer is empty
end macro;

\* The foreign writer: a poll of `write` from another thread.
fair process writer = "W"                                                    \* weakly fair, like every process
variables refused = FALSE;                                                   \* the socket refused this write once
begin                                                                        \* the writer's steps:
W_come:                                                                      \* `take_foreign`: intent up, with a locked instruction
    if ~shared then                                                          \* a biased side:
        drain();                                                             \* the locked instruction drains the buffer
        intent := TRUE;                                                      \* intent lands in memory
W_fence:                                                                     \* the heavy fence, then `own` (the owner holds nothing now)
        bufW := Append(bufW, "shared");                                      \* own is 0: the side goes shared (a plain store)
    end if;                                                                  \* a shared side goes to the word at once
W_take:                                                                      \* the word, with a locked instruction
    await word = "free";                                                     \* free
    drain();                                                                 \* the buffer lands first
    word := "W";                                                             \* the writer holds the side
W_send:                                                                      \* the write
    if refused then                                                          \* refused before: the edge came, room is there
        done := TRUE;                                                        \* the write goes out
    else                                                                     \* the first try:
        either                                                               \* the socket takes it:
            done := TRUE;                                                    \* the write goes out
        or                                                                   \* the socket is full:
            refused := TRUE;                                                 \* refused,
            blocked := TRUE;                                                 \* the side waits for an edge,
            nospace := TRUE;                                                 \* and the kernel owes one after room comes
        end either;                                                          \* the write is answered
    end if;                                                                  \* the step is done
W_publish:                                                                   \* `releasing`: whether an edge is wanted, a plain store
    if blocked then bufW := Append(bufW, "busy"); end if;                    \* not idle: buffered
W_release:                                                                   \* the word's release, a locked instruction
    drain();                                                                 \* the buffer lands first
    if ready then                                                            \* the driver left READY:
        ready := FALSE;                                                      \* taken,
        blocked := FALSE;                                                    \* served: room came
        goto W_send;                                                         \* and the write goes again, still held
    else                                                                     \* nothing left:
        word := "free";                                                      \* the side goes free
    end if;                                                                  \* let go
W_wait:                                                                      \* the poll waits for its wake
    if ~done then                                                            \* the write did not go out:
        await ~blocked;                                                      \* until the driver serves the edge
        goto W_take;                                                         \* then it polls again
    end if;                                                                  \* done
end process;                                                                 \* end of the writer

\* The kernel: room comes after a refused write, and it posts the edge.
fair process kernel = "K"                                                    \* weakly fair, like every process
begin                                                                        \* the kernel's steps:
K_spurious:                                                                  \* an edge for nobody, first
    if Spurious then edge := TRUE; end if;                                   \* room reported with a read's edge
K_post:                                                                      \* an edge owed, or none will be
    await nospace \/ pc["W"] = "Done";                                       \* owed, or the write is done
    if nospace then                                                          \* owed:
        nospace := FALSE;                                                    \* paid,
        edge := TRUE;                                                        \* as a write edge for the driver
    end if;                                                                  \* the kernel is done
end process;                                                                 \* end of the kernel

\* The owner thread's driver: `on_event` for a write edge.
fair process driver = "D"                                                    \* weakly fair, like every process
begin                                                                        \* the driver's steps:
D_wait:                                                                      \* the next edge
    await edge \/ (pc["W"] = "Done" /\ pc["K"] = "Done");                    \* an edge, or none will come
    if edge then                                                             \* an edge:
        edge := FALSE;                                                       \* it is taken
    else                                                                     \* none will come:
        goto Done;                                                           \* the driver is done
    end if;                                                                  \* the edge is in hand
D_look:                                                                      \* `write_wants_no_edge`: plain loads, from memory
    \* MUTATION skip_idle_only: the look leaves out the side's mode and `intent`, `write_idle` alone.
    \* MUTATION skip_no_shared: the look leaves out the side's mode.
    if idle
       /\ (~intent \/ Mutation = "skip_idle_only")
       /\ (~shared \/ Mutation \in {"skip_idle_only", "skip_no_shared"}) then   \* only its own thread held it, idle:
        skipped := TRUE;                                                     \* (record) the edge is passed over
        goto D_wait;                                                         \* and the driver goes on
    end if;                                                                  \* else the edge is served
D_drive:                                                                     \* `Side::drive` through the word
    if word = "free" then                                                    \* free: the driver holds it for the step,
        blocked := FALSE;                                                    \* serves it (the blocked write may go again),
                                                                             \* lets go and wakes the writer
    else                                                                     \* held by the writer:
        ready := TRUE;                                                       \* READY is left for it
    end if;                                                                  \* the edge is served
    goto D_wait;                                                             \* the next edge
end process;                                                                 \* end of the driver

\* The memory: the oldest store of the writer's buffer lands at any moment.
fair process flusher = "F"                                                   \* weakly fair, like every process
begin                                                                        \* the flusher's step:
F_land:                                                                      \* the oldest store lands
    while bufW # <<>> \/ pc["W"] # "Done" do                                 \* while stores may come
        await bufW # <<>> \/ pc["W"] = "Done";                               \* one is buffered, or none will be
        if bufW # <<>> then                                                  \* the oldest:
            if Head(bufW) = "shared" then shared := TRUE;                    \* the mode switch lands,
            else idle := FALSE;                                              \* or `write_idle` lands
            end if;                                                          \* in memory
            bufW := Tail(bufW);                                              \* and leaves the buffer
        end if;                                                              \* one store
    end while;                                                               \* nothing more
end process;                                                                 \* end of the flusher
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* The foreign write goes out in the end: no edge it waits for is passed over.
WriteGoes == <>done                                                          \* eventually done

\* Witness: the driver passes over an edge (one that came for nobody).
NoSkip == ~skipped                                                           \* claimed never to happen
=============================================================================

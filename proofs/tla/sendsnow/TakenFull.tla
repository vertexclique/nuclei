----------------------------- MODULE TakenFull -----------------------------
(***************************************************************************)
(* A stream's owner whose straight send found the socket full, when the    *)
(* shard is taken from its thread before the rest of the same poll (src/  *)
(* io/source/write/calls.rs: `Source::poll_write_bufs`, which first tries  *)
(* `Source::send_as_owner` under the owner's biased hold and records a     *)
(* full socket in `asked`, then the owner's poll `owner_poll!` or, once    *)
(* its call finds the shard taken, `Source::poll_write_any`, whose step   *)
(* `Source::write_step` skips its own ask of the kernel when told the      *)
(* socket was full and only marks the side blocked,                        *)
(* `WriteState::found_full` in src/io/source/write.rs; the epoll driver's *)
(* look `Source::write_wants_no_edge` and `Source::on_event` in            *)
(* src/io/source.rs; the take, `take` in src/rt/handoff/monitor.rs).        *)
(*                                                                          *)
(* Who acts. The owner's task, which writes one byte in one poll on the    *)
(* thread that ran the stream's home shard. The monitor, which may take   *)
(* the shard from that thread once, in the moment between the owner's     *)
(* straight send and the rest of its poll (the straight send's mark ended *)
(* as it settled, and the poll's next call is not marked yet). The        *)
(* shard's driver: on the owner's own thread between its polls, or on the *)
(* new thread at any time once the shard was taken. The kernel, which     *)
(* frees room once (the peer reads) and posts one write edge for each send *)
(* it refused since the last edge (epoll's edge trigger).                 *)
(*                                                                          *)
(* What can happen. The straight send's hold publishes nothing as it lets *)
(* go, so the side still says it was only ever held by its owner and idle *)
(* (`write_wants_no_edge`): a driver given an edge then passes over it.   *)
(* Without a take that is harmless, for no driver runs during the poll and *)
(* the poll's own release says the side is not idle before the driver's  *)
(* next look. With a take, the new thread's driver may take the edge the  *)
(* refusal owed and pass over it while the old thread is between its      *)
(* straight send and its poll; the old thread's poll then goes the way of *)
(* any other thread (its call finds the shard taken), which makes the     *)
(* side shared. If that way trusted the straight send's answer, it would  *)
(* block the side with no ask of its own, and the edge it waits for was   *)
(* already passed over: the write would wait for good. So that way asks   *)
(* the kernel again under its own hold.                                    *)
(*                                                                          *)
(* What must never happen. The side waits for an edge that nothing owes   *)
(* or queues (`BlockedOwed`). The byte is never sent (`Sent`).            *)
(*                                                                          *)
(* What is abstract. Holds exclude each other (proofs/tla/side and        *)
(* side_biased check that), so a hold is one step; the mode switch of a   *)
(* foreign hold is in memory before it holds (EdgeSkip.tla). The take is   *)
(* one step (proofs/tla/handoff). One byte, one room, one take.           *)
(***************************************************************************)
EXTENDS Integers, TLC                                                        \* numbers and TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Handoffs,   \* 0, or 1 when the monitor may take the shard in the poll's moment between its calls
    Mutation    \* "none", or the name of the one rule this run takes away

(* --algorithm TakenFull
variables                                                                    \* the shared state:
    room = FALSE,             \* the socket has room: the kernel takes a send
    refused = FALSE,          \* a send was refused since the kernel's last edge: one is owed once room comes
    edge = FALSE,             \* a write edge waits for the driver
    biased = TRUE,            \* the side was only ever held by its owner (not shared, no foreign thread came)
    idle = TRUE,              \* the side's last release said it waits for nothing an edge brings
    blocked = FALSE,          \* the side waits for an edge
    ready = FALSE,            \* READY, an edge left for the holder of the side
    held = FALSE,             \* someone holds the side through its word
    taken = FALSE,            \* the shard was taken from the owner's thread
    handoffs = 0,             \* takes made
    sent = FALSE,             \* the byte went out
    skipped = FALSE;          \* (record) the driver passed over an edge

define                                                                       \* names the properties use:
    \* The side waits for an edge only while one is owed, queued or left for its holder.
    BlockedOwed == blocked => (refused \/ edge \/ ready)                      \* a blocked side has an edge to come
    \* The byte goes out in the end.
    Sent == <>sent                                                           \* eventually sent
    \* Witness: the driver passes over an edge.
    NoSkip == ~skipped                                                       \* claimed never to happen
end define;                                                                  \* end of the names

\* The owner's task: one write of one byte, in one poll, then waits for its wake if the side blocked.
fair process owner = "O"                                                     \* weakly fair, like every process
variables asked = FALSE;                                                     \* the straight send found the socket full
begin                                                                        \* the owner's steps:
O_fast:                                                                      \* `send_as_owner`: the straight send under the biased hold
    \* The hold publishes nothing as it lets go: the side still says biased and idle.
    if room then                                                             \* the kernel takes the byte:
        sent := TRUE;                                                        \* it goes out,
        goto Done;                                                           \* and the write is over
    else                                                                     \* the socket is full:
        refused := TRUE;                                                     \* refused, so the kernel owes an edge once room comes,
        asked := TRUE;                                                       \* and the poll's way is told
    end if;                                                                  \* the straight send's mark ends as it settles
O_enter:                                                                     \* the poll's next call: `owner_poll!`'s mark, or the way of any thread
    if ~taken then goto O_owner; end if;                                     \* the shard is still this thread's: the owner's poll
O_foreign:                                                                   \* the shard was taken: `poll_write_any`, the way of any other thread
    \* A foreign take: the side goes shared before it holds (its mode is in memory first).
    await ~held;                                                             \* the side is free
    biased := FALSE;                                                         \* shared, for good
    held := TRUE;                                                            \* held through the word
O_fstep:                                                                     \* `write_step`
    \* The way of any thread asks the kernel itself under its own hold.
    \* MUTATION blind_found_full: it takes the straight send's answer and blocks the side with no ask.
    if asked /\ Mutation = "blind_found_full" then                           \* (mutated) told full:
        blocked := TRUE;                                                     \* `found_full`, with no ask of the kernel
    elsif room then                                                          \* the kernel takes the byte:
        sent := TRUE;                                                        \* it goes out
    else                                                                     \* the socket is full:
        refused := TRUE;                                                     \* refused, so an edge is owed,
        blocked := TRUE;                                                     \* and the side waits for it
    end if;                                                                  \* the step is done
O_frel:                                                                      \* the release: what it publishes, then the word
    idle := ~blocked;                                                        \* `releasing`: idle only with nothing to wait for
    if ready then                                                            \* the driver left READY meanwhile:
        ready := FALSE;                                                      \* taken,
        blocked := FALSE;                                                    \* served: room came
    end if;                                                                  \* (its retry comes with the wake below)
    held := FALSE;                                                           \* let go
    goto O_wait;                                                             \* to the wait, if blocked
O_owner:                                                                     \* the owner's poll: the biased hold
    if asked then                                                            \* told full by the straight send:
        blocked := TRUE;                                                     \* `found_full`: no driver ran during the poll
    elsif room then                                                          \* else its own ask: taken
        sent := TRUE;                                                        \* the byte goes out
    else                                                                     \* refused
        refused := TRUE;                                                     \* an edge is owed,
        blocked := TRUE;                                                     \* and the side waits for it
    end if;                                                                  \* the step is done
    idle := ~blocked;                                                        \* `releasing`: not idle while blocked
O_wait:                                                                      \* the poll returned
    if ~sent then                                                            \* the byte is still to go:
        await ~blocked;                                                      \* until the driver serves an edge
O_retry:                                                                     \* woken: the write goes again, through the word
        await ~held;                                                         \* the side is free
        if room then                                                         \* the kernel takes the byte:
            sent := TRUE;                                                    \* it goes out
        else                                                                 \* refused again:
            refused := TRUE;                                                 \* an edge is owed,
            blocked := TRUE;                                                 \* and the side waits for it,
            idle := FALSE;                                                   \* which its release says
            goto O_wait;                                                     \* and the poll waits again
        end if;                                                              \* the write is answered
    end if;                                                                  \* done
end process;                                                                 \* end of the owner

\* The monitor: the take, in the moment between the straight send and the poll's next call.
fair process monitor = "M"                                                   \* weakly fair, like every process
begin                                                                        \* the monitor's step:
M_take:                                                                      \* the take
    \* The monitor sees the poll in the moment between its calls, or the poll past it.
    await Handoffs = 0 \/ pc["O"] \notin {"O_fast"};                          \* a take is allowed, and the poll's first call made
    if Handoffs > 0 /\ pc["O"] = "O_enter" then                              \* the poll sits between its calls:
        either                                                               \* the take lands
            taken := TRUE;                                                   \* the shard is the new thread's
            handoffs := handoffs + 1;                                        \* counted
        or                                                                   \* or it does not
            skip;                                                            \* (the poll comes back in first)
        end either;                                                          \* the monitor is done
    end if;                                                                  \* the moment passed: no take
end process;                                                                 \* end of the monitor

\* The kernel: room comes once, and each send refused since the last edge is owed one.
fair process kernel = "K"                                                    \* weakly fair, like every process
begin                                                                        \* the kernel's steps:
K_room:                                                                      \* the peer reads: room comes
    await refused;                                                           \* after a refusal
    room := TRUE;                                                            \* room
K_post:                                                                      \* the edges owed
    while pc["O"] # "Done" do                                                \* for as long as a refusal may come
        await refused \/ pc["O"] = "Done";                                   \* a send refused, or none will be
        if refused then                                                      \* owed:
            refused := FALSE;                                                \* paid,
            edge := TRUE;                                                    \* as a write edge for the driver
        end if;                                                              \* the next
    end while;                                                               \* nothing more
end process;                                                                 \* end of the kernel

\* The shard's driver: on the owner's thread between its polls, or on the new thread once taken.
fair process driver = "D"                                                    \* weakly fair, like every process
begin                                                                        \* the driver's steps:
D_loop:                                                                      \* the next edge
    while pc["O"] # "Done" \/ edge do                                        \* for as long as an edge may come
        await edge                                                           \* an edge,
              /\ (taken \/ pc["O"] \in {"O_wait", "O_retry", "Done"});       \* where the driver runs now
        edge := FALSE;                                                       \* taken
D_look:                                                                      \* `write_wants_no_edge`
        if biased /\ idle then                                               \* only its owner held it, idle:
            skipped := TRUE;                                                 \* (record) passed over
        elsif held then                                                      \* held: READY for the holder
            ready := TRUE;                                                   \* left
        else                                                                 \* free: served
            blocked := FALSE;                                                \* the blocked write may go again
        end if;                                                              \* the edge is done
    end while;                                                               \* nothing more
end process;                                                                 \* end of the driver
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

------------------------------ MODULE RingInc ------------------------------
(***************************************************************************)
(* A stream's multishot receives on a receive group filled in pieces        *)
(* (IOU_PBUF_RING_INC): src/rt/shard/recv.rs (`RecvGroup::hand_out`,       *)
(* `settle`, `take_back`, `charge`), src/rt/shard/ring.rs (`arrived`), and *)
(* src/io/source/ring.rs (`multishot_datagrams`), at the grain of the      *)
(* kernel's completions, the ring thread's reaping and the reader's polls. *)
(*                                                                          *)
(* Who acts. A peer writes messages. The kernel fills the group's buffers  *)
(* for an armed multishot receive. The ring's thread reaps completions,    *)
(* hands each piece to the source as a chunk, stops a receive past its    *)
(* cap, takes back chunks given back and gives each buffer to the kernel   *)
(* again when it may. The reader takes chunks in order and drops each one *)
(* (its buffer goes on the return queue).                                  *)
(*                                                                          *)
(* What can happen. The kernel keeps a buffer while it has room: each      *)
(* message goes into the buffer at the head of its ring, after the bytes   *)
(* already there, and its completion says BUF_MORE while the buffer has   *)
(* room left; the completion that fills it does not, and the kernel moves *)
(* to the next buffer. A receive that ends leaves a buffer partly filled  *)
(* at the head, and the next receive goes on filling it. For each buffer  *)
(* the ring's thread counts where the next piece starts, the chunks of it *)
(* out, and whether the kernel is done with it; it gives the buffer back  *)
(* to the kernel only when the kernel is done and no chunk of it is out.  *)
(* A chunk counts at least `MinCharge` against its receive's cap. A        *)
(* datagram socket does not use the group while it is filled in pieces:  *)
(* the kernel would cut a datagram to the room left in a buffer, so its   *)
(* receives take a buffer of the pool, one datagram each.                 *)
(*                                                                          *)
(* What must never happen. A message is lost, doubled or reordered. A      *)
(* buffer is given to the kernel again while the kernel still holds it or *)
(* a chunk of it is out (its bytes would be written under a reader). A    *)
(* buffer is missing from the kernel's ring once the stream ended. A       *)
(* receive holds more chunks than its cap allows. A datagram is cut.       *)
(*                                                                          *)
(* What is abstract. A stream's message is one unit of a buffer, a        *)
(* datagram two; the completion queue holds `CqCap` completions; the      *)
(* reader takes a whole chunk at once; the return queue                    *)
(* (a SegQueue, and the ring thread's local list) is taken back in any    *)
(* order; a receive into a pool buffer takes one message, whole. The       *)
(* lent path and the accept are RingMulti.tla's.                            *)
(***************************************************************************)
EXTENDS Integers, Sequences, FiniteSets, TLC                                 \* numbers, sequences, set sizes, TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    N,          \* messages the peer writes
    Total,      \* buffers in the group
    Size,       \* units in a buffer
    Cap,        \* units of charge a receive may hold before the ring stops it
    MinCharge,  \* the least a chunk counts against its receive's cap
    Burst,      \* arrivals the kernel may still make for a receive before a cancel takes it
    CqCap,      \* completions the kernel may post ahead of the ring's reaping (the completion queue)
    Datagram,   \* TRUE: the socket is a datagram socket, each message two units
    Mutation    \* "none", or the name of the one rule this run takes away

Bufs == 1..Total                                                             \* the group's buffers
MsgLen == IF Datagram THEN 2 ELSE 1                                          \* the units of one message
End == -1                                                                    \* what a pool receive brings at the stream's end
\* What a chunk of `len` units counts against its receive's cap.
\* MUTATION charge_len: a chunk counts its length only.
Charge(len) == IF Mutation = "charge_len" THEN len                           \* (mutated) its length,
               ELSE IF len > MinCharge THEN len ELSE MinCharge               \* else at least the minimum
\* The bids in sequence `s`.
Range(s) == {s[i] : i \in DOMAIN s}                                          \* the set of its entries

(* --algorithm RingInc                                                         \* the algorithm, in PlusCal
variables                                                                    \* the shared state:
    sock = <<>>,          \* messages in the socket, by id
    written = 0,          \* messages the peer wrote
    kring = [i \in 1..Total |-> i],   \* the kernel's ring of buffers, the one it fills first at the head
    koff = 0,             \* units of the head buffer the kernel filled
    content = [b \in Bufs |-> [u \in 1..Size |-> 0]],   \* the message id each unit of each buffer holds
    fills = [b \in Bufs |-> [used |-> 0, live |-> 0, done |-> FALSE]],   \* the ring thread's `Fill` per buffer
    alive = [b \in Bufs |-> 0],       \* (ghost) chunks of each buffer handed out and not dropped by the reader
    handouts = [b \in Bufs |-> 0],    \* (ghost) pieces of each buffer handed out since it was last given back
    armed = FALSE,        \* a multishot receive is in the kernel
    stop = FALSE,         \* a cancel of it is on its way
    batch = 0,            \* arrivals the kernel made since the cancel came
    cq = <<>>,            \* completions not reaped yet
    rid = 0,              \* the receive in flight, by number
    ended = 0,            \* the last receive whose end the ring reaped (its op, and its count, are gone)
    held = 0,             \* the charge of the chunks of receive `rid` not taken back
    chunks = 0,           \* the chunks of receive `rid` not taken back
    stopping = FALSE,     \* the ring cancelled the receive in flight
    chunkq = <<>>,        \* chunks handed to the source, not taken in by the side
    mailbox = "none",     \* the receive's end, not taken by the side
    returned = <<>>,      \* chunks dropped, on the return queue: [b, rid, len]
    ahead = <<>>,         \* the side's read-ahead: chunks [b, pos, len, rid], or [pool, m]
    got = <<>>,           \* what the reader received, in order
    out = "none",         \* the side's operation out: "none", "multi", "pool"
    opsOut = 0,           \* the side's operations out at once
    dry = FALSE,          \* the next receive takes a pool buffer
    eof = FALSE,          \* the side saw the stream's end
    poolRes = 0,          \* what a pool receive brought: an id, End, or 0 for nothing
    badAdd = FALSE,       \* (record) a buffer went back while the kernel held it or a chunk of it was out
    cut = FALSE,          \* (record) a datagram was cut to the room left in a buffer
    pieces = FALSE,       \* (record) a buffer was handed out in two pieces or more
    readds = 0;           \* (record) buffers given back to the kernel

define                                                                       \* names the code and the properties use:
    PeerDone == written = N                                                  \* the peer wrote everything
    ReaderDone == Len(got) = N                                               \* the reader has every message
end define;                                                                  \* end of the names

\* Gives buffer `b` to the kernel again (`settle`): its `Fill` starts over, and it joins the end of the kernel's ring.
macro ReAdd(b) begin                                                         \* its steps:
    badAdd := badAdd \/ b \in Range(kring) \/ alive[b] > 0;                  \* (record) the kernel holds it, or a chunk is out
    kring := Append(kring, b);                                               \* the kernel may fill it again
    fills[b] := [used |-> 0, live |-> 0, done |-> FALSE];                    \* from its start
    handouts[b] := 0;                                                        \* (ghost) no piece out of this fill
    readds := readds + 1;                                                    \* (record)
end macro;                                                                   \* end of the give-back

fair process peer = "peer"                                                   \* weakly fair, like every process
begin                                                                        \* the peer's steps:
P_write:                                                                     \* one message
    while written < N do                                                     \* while it has messages
        written := written + 1;                                              \* the next
        sock := Append(sock, written);                                       \* into the socket
    end while;                                                               \* all written
end process;                                                                 \* end of the peer

\* The kernel: a multishot receive's arrivals into the group and its end; a pool receive's one message.
fair process kernel = "kernel"                                               \* weakly fair, like every process
variables units = 0;                                                         \* the units the message takes in the head buffer
begin                                                                        \* the kernel's steps:
K_loop:                                                                      \* one completion
    while ~(ReaderDone /\ cq = <<>>) do                                      \* while the reader may want more
        either                                                               \* an arrival:
            await armed /\ sock # <<>> /\ kring # <<>> /\ (~stop \/ batch < Burst)   \* bytes, a buffer, no cancel yet,
                  /\ Len(cq) < CqCap;                                        \* and room in the completion queue
            units := IF MsgLen <= Size - koff THEN MsgLen ELSE Size - koff;  \* the units it gets in the head buffer's room
            cut := cut \/ units < MsgLen;                                    \* (record) a datagram cut short
            content[Head(kring)] := [u \in 1..Size |->                       \* the message's id goes
                IF u > koff /\ u <= koff + units THEN Head(sock)             \* into the units after those filled,
                ELSE content[Head(kring)][u]];                               \* the rest as they were
            cq := Append(cq, [k |-> "arrival", b |-> Head(kring), pos |-> koff + 1,   \* a completion for the piece,
                              len |-> units, more |-> koff + units < Size]);  \* BUF_MORE while room is left
            if koff + units < Size then koff := koff + units;                \* room left: the kernel keeps the buffer
            else koff := 0; kring := Tail(kring);                            \* full: the kernel moves to the next
            end if;                                                          \* the buffer is filled that far
            sock := Tail(sock);                                              \* the message is out of the socket
            if stop then batch := batch + 1; end if;                         \* counted after a cancel came
        or                                                                   \* bytes and no buffer: the receive ends dry
            await armed /\ sock # <<>> /\ kring = <<>>;                      \* the ring is empty
            armed := FALSE;                                                  \* the receive is over
            cq := Append(cq, [k |-> "end", why |-> "dry"]);                  \* its last completion
        or                                                                   \* the cancel takes the receive
            await armed /\ stop;                                             \* a cancel came
            armed := FALSE;                                                  \* the receive is over
            cq := Append(cq, [k |-> "end", why |-> "cancel"]);               \* its last completion
        or                                                                   \* the stream ended and nothing is left
            await armed /\ sock = <<>> /\ PeerDone;                          \* nothing more will come
            armed := FALSE;                                                  \* the receive is over
            cq := Append(cq, [k |-> "end", why |-> "eof"]);                  \* its last completion
        or                                                                   \* a receive into a pool buffer: one message, whole
            await out = "pool" /\ poolRes = 0 /\ (sock # <<>> \/ PeerDone);  \* a message, or the end
            if sock # <<>> then poolRes := Head(sock); sock := Tail(sock);   \* the message
            else poolRes := End;                                             \* or the end
            end if;                                                          \* one completion
        end either;                                                          \* the next
    end while;                                                               \* the reader has all
end process;                                                                 \* end of the kernel

\* The ring's thread (`arrived`, `take_back`): reaps completions in order, takes back chunks given back.
fair process ring = "ring"                                                   \* weakly fair, like every process
variables c = [k |-> "none"], back = [b |-> 0];                              \* the completion reaped; the chunk taken back
begin                                                                        \* the ring thread's steps:
R_loop:                                                                      \* the next piece of work
    while ~(ReaderDone /\ cq = <<>> /\ returned = <<>>) do                   \* while anything may come
        either                                                               \* a completion:
            await cq # <<>>;                                                 \* one is there
            c := Head(cq);                                                   \* the oldest
            cq := Tail(cq);                                                  \* is reaped
            if c.k = "arrival" then                                          \* a piece (`hand_out`):
                fills[c.b] := [used |-> c.pos + c.len - 1,                   \* the next piece starts after it,
                               live |-> fills[c.b].live + 1,                 \* one more chunk of it out,
                               done |-> ~c.more];                            \* and the kernel done once BUF_MORE is off
                alive[c.b] := alive[c.b] + 1;                                \* (ghost) a chunk out
                handouts[c.b] := handouts[c.b] + 1;                          \* (ghost) one more piece of this fill
                pieces := pieces \/ handouts[c.b] >= 2;                      \* (record) a second piece of one fill
                held := held + Charge(c.len);                                \* counted against the receive's cap
                chunks := chunks + 1;                                        \* one more chunk of the receive
                chunkq := Append(chunkq, [b |-> c.b, pos |-> c.pos, len |-> c.len, r |-> rid]);   \* to the source
                if held > Cap /\ ~stopping then                              \* past the cap:
                    stopping := TRUE;                                        \* the ring stops the receive
                    stop := TRUE;                                            \* with a cancel
                    batch := 0;                                              \* that the kernel takes in its time
                end if;                                                      \* the cap is looked at
            else                                                             \* the receive's end:
                mailbox := c.why;                                            \* posted to the side
                ended := rid;                                                \* the receive's op is gone
                held := 0;                                                   \* the next receive starts at nothing
                chunks := 0;                                                 \* and with no chunk
                stopping := FALSE;                                           \* no stop under way
                stop := FALSE;                                               \* no cancel either
            end if;                                                          \* the completion is handled
R_dirty:                                                                     \* (mutated) a give-back on the kernel's last piece
            \* MUTATION readd_on_done: a buffer goes back as soon as the kernel is done with it, chunks of it out or not.
            if c.k = "arrival" /\ ~c.more /\ Mutation = "readd_on_done" then ReAdd(c.b); end if;   \* (mutated)
        or                                                                   \* a chunk given back (`take_back`), in any order:
            await returned # <<>>;                                           \* the return queue has one
            with i \in DOMAIN returned do                                    \* any of them
                back := returned[i];                                         \* taken,
                returned := [j \in 1..Len(returned) - 1 |->                  \* off the queue
                             IF j < i THEN returned[j] ELSE returned[j + 1]];  \* (the others keep their order)
            end with;                                                        \* one chunk
            if back.r = rid /\ ended # rid then                              \* of the receive in flight:
                held := held - Charge(back.len);                             \* its charge comes off
                chunks := chunks - 1;                                        \* and one chunk fewer
            end if;                                                          \* a past receive's is gone already
            fills[back.b].live := IF fills[back.b].live > 0                  \* one chunk of the buffer fewer out
                                  THEN fills[back.b].live - 1 ELSE 0;        \* (never below zero, as `saturating_sub`)
R_settle:                                                                    \* `settle`
            \* MUTATION readd_with_more: a buffer goes back when its last chunk comes back, done or not.
            if fills[back.b].live = 0                                        \* no chunk of it out,
               /\ (fills[back.b].done \/ Mutation = "readd_with_more") then  \* and the kernel done with it:
                ReAdd(back.b);                                               \* it goes back to the kernel
            end if;                                                          \* else it waits
        end either;                                                          \* the next
    end while;                                                               \* nothing more will come
end process;                                                                 \* end of the ring's thread

\* The reader: each poll applies what came, then takes the next message or asks the driver for more.
fair process reader = "reader"                                               \* weakly fair, like every process
variables h = [b |-> 0];                                                     \* the chunk it takes
begin                                                                        \* the reader's steps:
D_loop:                                                                      \* a poll
    while ~ReaderDone /\ ~(eof /\ ahead = <<>>) do                           \* while messages may come
        ahead := ahead \o chunkq;                                            \* the chunks that came, in order,
        chunkq := <<>>;                                                      \* taken in
        if mailbox # "none" then                                             \* then the end after them:
            if out = "multi" then out := "none" || opsOut := opsOut - 1; end if;   \* the receive is over,
            if mailbox = "dry" then dry := TRUE; end if;                     \* a dry end sends the next to the pool,
            if mailbox = "eof" then eof := TRUE; end if;                     \* the stream's end is seen,
            mailbox := "none";                                               \* and the end is taken
        end if;                                                              \* what came is in
D_res:                                                                       \* what a pool receive brought
        if poolRes # 0 then                                                  \* a pool receive completed:
            if poolRes = End then eof := TRUE;                               \* the end,
            else ahead := Append(ahead, [b |-> 0, pos |-> poolRes, len |-> MsgLen, r |-> 0]);   \* or a message, whole
            end if;                                                          \* in the read-ahead
            dry := FALSE;                                                    \* the next receive may use the group
            out := "none";                                                   \* no operation out
            opsOut := opsOut - 1;                                            \* one fewer
            poolRes := 0;                                                    \* taken
        end if;                                                              \* the result is in
D_take:                                                                      \* the next message, or a receive
        if ahead # <<>> then                                                 \* a message read ahead:
            h := Head(ahead);                                                \* the oldest,
            if h.b = 0 then got := Append(got, h.pos);                       \* a pool buffer's message,
            else got := Append(got, content[h.b][h.pos]);                    \* or what its chunk's buffer holds now
            end if;                                                          \* is received
            ahead := Tail(ahead);                                            \* and leaves the read-ahead
            if h.b # 0 then                                                  \* a chunk is dropped:
                returned := Append(returned, [b |-> h.b, r |-> h.r, len |-> h.len]);   \* onto the return queue,
                alive[h.b] := alive[h.b] - 1;                                \* (ghost) no longer out
            end if;                                                          \* the chunk is gone
        elsif ~eof /\ out = "none" then                                      \* nothing ahead and nothing out:
            \* MUTATION dgram_multishot: a datagram socket arms a multishot receive on the group filled in pieces.
            if dry \/ (Datagram /\ Mutation # "dgram_multishot") then        \* a dry end, or a datagram socket:
                out := "pool";                                               \* one receive into a buffer of the pool
            else                                                             \* else a multishot receive:
                out := "multi";                                              \* out,
                armed := TRUE;                                               \* in the kernel,
                stop := FALSE;                                               \* not stopped,
                rid := rid + 1;                                              \* a new receive,
                held := 0;                                                   \* holding nothing yet
                chunks := 0;                                                 \* and no chunk
            end if;                                                          \* the receive is out
            opsOut := opsOut + 1;                                            \* one operation out
        else                                                                 \* waits for the driver
            await chunkq # <<>> \/ mailbox # "none" \/ poolRes # 0;          \* what comes next
        end if;                                                              \* the poll is over
    end while;                                                               \* the reader has all
end process;                                                                 \* end of the reader
end algorithm; *)                                                            \* end of the algorithm
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* The reader gets the messages in order, none lost, none twice.
InOrder == got = [i \in 1..Len(got) |-> i]                                   \* the first so many, in order

\* No buffer goes back to the kernel while the kernel holds it or a chunk of it is out, and the kernel's ring never names
\* a buffer twice.
NoReuse ==                                                                   \* holds when both parts do:
    /\ ~badAdd                                                               \* never a give-back too soon
    /\ \A i, j \in DOMAIN kring : i # j => kring[i] # kring[j]               \* never a buffer twice in the ring

AllDone == \A p \in DOMAIN pc : pc[p] = "Done"                               \* every process finished

\* Once everything is done, every buffer is back in the kernel's ring and no chunk is out.
BuffersBack == AllDone => (Range(kring) = Bufs /\ \A b \in Bufs : fills[b].live = 0)   \* all back

\* A receive holds at most its cap, at the least charge per chunk, then the completions the kernel had posted when the
\* ring passed it (the code bounds a receive from that batch on, not within it) and what the kernel took before the
\* cancel reached it.
CapHeld == chunks * MinCharge <= Cap + MinCharge * (CqCap + Burst)           \* chunks bounded by the cap

\* A datagram is never cut to the room left in a buffer.
NoCut == ~cut                                                                \* every datagram whole

\* `Termination` (every process ends, so the reader got every message) is the one the translator writes.

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A buffer is handed out in two pieces or more in one fill.
NoPieces == ~pieces                                                          \* never two pieces
\* A buffer goes back to the kernel.
NoReAdd == readds = 0                                                        \* never given back
=============================================================================

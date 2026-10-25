----------------------------- MODULE RingMulti -----------------------------
(***************************************************************************)
(* A stream's read side on its shard's ring with multishot receives       *)
(* (src/io/source/read/ops.rs, src/io/source/read.rs, src/rt/shard/ring.rs, *)
(* src/rt/shard/recv.rs), at the grain of the kernel's arrivals, the ring  *)
(* thread's completions and the reader's polls.                          *)
(*                                                                          *)
(* In plain words: a peer writes messages. The reader's first read arms   *)
(* one multishot receive; the kernel fills a buffer of the shard's group  *)
(* for each message as it comes and posts an arrival, more to come. The   *)
(* ring's thread hands each arrival to the source as a chunk, in order,   *)
(* and counts the bytes the receive holds: past the cap it cancels it.   *)
(* The receive ends (cancelled, the group dry, the stream's end) with one *)
(* last completion, which the side applies after the chunks before it.   *)
(* A reader takes chunks in order and gives each buffer back, and arms a  *)
(* new receive only with nothing read ahead; after a dry end its next     *)
(* receive takes a buffer of the pool instead. A reader of lent buffers   *)
(* stops an armed receive when it has nothing read ahead, and reads the   *)
(* next message straight into its buffer.                                 *)
(*                                                                          *)
(* A multishot accept is the same shape (src/rt/shard/ring.rs,            *)
(* `accepted`): its arrivals are connections instead of buffers, counted  *)
(* against the accept's cap, and the side asks for another accept only    *)
(* once its tasks took them; so a message here stands for a connection   *)
(* too, and a buffer for a connection the accept handed over.             *)
(*                                                                          *)
(* What is modelled step by step: the kernel's arrivals, its handling of  *)
(* a cancel (up to `Burst` more arrivals may come first), its dry and end *)
(* answers; the ring's reaping, its cap and its taking back of buffers;   *)
(* each poll of the reader (the side's guard as one atomic step: the side *)
(* family checks it). What is abstract: a message is one buffer's worth.  *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    N,         \* messages the peer writes
    Total,     \* buffers in the group
    Cap,       \* buffers one receive may hold before the ring stops it
    Burst,     \* arrivals the kernel may still make for a receive before a cancel takes it
    Lent,      \* TRUE: the reader reads into lent buffers
    Mutation   \* "none", or the name of the one rule this run takes away

(* --algorithm RingMulti
variables
    sock = <<>>,          \* bytes in the socket, as message ids
    written = 0,          \* messages the peer wrote
    free = Total,         \* buffers the kernel may fill
    armed = FALSE,        \* a multishot receive is in the kernel
    stop = FALSE,         \* a cancel of it is on its way
    batch = 0,            \* arrivals the kernel made since the cancel came
    cq = <<>>,            \* completions not reaped yet
    held = 0,             \* buffers of the receive in flight not given back
    stopping = FALSE,     \* the ring cancelled the receive in flight
    chunkq = <<>>,        \* chunks handed to the source, not taken by the side
    mailbox = "none",     \* the receive's end, not taken by the side
    returned = 0,         \* buffers given back, not taken back by the ring
    ahead = <<>>,         \* the side's read-ahead: [m |-> id, chunk |-> BOOLEAN]
    got = <<>>,           \* what the reader received, in order
    out = "none",         \* the side's operation out: "none", "multi", "pool", "lent"
    opsOut = 0,           \* the side's operations out at once (record)
    dry = FALSE,          \* the next receive takes a pool buffer
    eof = FALSE,          \* the side saw the stream's end
    poolRes = 0,          \* what a pool or lent receive brought: an id, End, or 0 for nothing
    armedWithAhead = FALSE, \* (record) a receive was armed with chunks unread
    dryEnds = 0,          \* (record) receives that ended dry
    stops = 0,            \* (record) receives the ring stopped
    lentGot = 0;          \* (record) messages read straight into a lent buffer

define
    \* What a pool or lent receive brings at the stream's end.
    End == -1
    PeerDone == written = N
    \* The reader has every message.
    ReaderDone == Len(got) = N
end define;

fair process peer = "peer"
begin
P_write:
    while written < N do
        written := written + 1;
        sock := Append(sock, written);
    end while;
end process;

\* The kernel: a multishot receive's arrivals and its end; a pool or lent
\* receive's one message.
fair process kernel = "kernel"
begin
K_loop:
    while ~(ReaderDone /\ cq = <<>>) do
        either
            \* An arrival: the head message into a buffer of the group.
            await armed /\ sock # <<>> /\ free > 0 /\ (~stop \/ batch < Burst);
            free := free - 1;
            cq := Append(cq, [k |-> "arrival", m |-> Head(sock)]);
            sock := Tail(sock);
            if stop then batch := batch + 1; end if;
        or
            \* Bytes and no buffer: the receive ends dry.
            await armed /\ sock # <<>> /\ free = 0;
            armed := FALSE;
            cq := Append(cq, [k |-> "end", why |-> "dry"]);
        or
            \* The cancel takes the receive.
            await armed /\ stop;
            armed := FALSE;
            cq := Append(cq, [k |-> "end", why |-> "cancel"]);
        or
            \* The stream ended and nothing is left.
            await armed /\ sock = <<>> /\ PeerDone;
            armed := FALSE;
            cq := Append(cq, [k |-> "end", why |-> "eof"]);
        or
            \* A receive into a pool or a lent buffer takes one message.
            await out \in {"pool", "lent"} /\ poolRes = 0 /\ (sock # <<>> \/ PeerDone);
            if sock # <<>> then
                poolRes := Head(sock);
                sock := Tail(sock);
            else
                poolRes := End;
            end if;
        end either;
    end while;
end process;

\* The ring's thread: reaps completions in order, hands arrivals to the
\* source, posts a receive's end, stops a receive past its cap, and takes
\* back the buffers given back.
fair process ring = "ring"
variables c = [k |-> "none"];
begin
R_loop:
    while ~(ReaderDone /\ cq = <<>> /\ returned = 0) do
        either
            await cq # <<>>;
            c := Head(cq);
            cq := Tail(cq);
            if c.k = "arrival" then
                held := held + 1;
                chunkq := Append(chunkq, c.m);
                \* MUTATION no_cap: the ring never stops a receive.
                if held > Cap /\ ~stopping /\ Mutation # "no_cap" then
                    stopping := TRUE;
                    stop := TRUE;
                    batch := 0;
                    stops := stops + 1;
                end if;
            else
                mailbox := c.why;
                held := 0;
                stopping := FALSE;
                stop := FALSE;
                if c.why = "dry" then dryEnds := dryEnds + 1; end if;
            end if;
        or
            \* `take_back`, before the ring's next wait.
            await returned > 0;
            free := free + returned;
            held := IF held >= returned THEN held - returned ELSE 0;
            returned := 0;
        end either;
    end while;
end process;

\* The reader: each poll applies what came (the chunks, then the end), then
\* takes the next message or asks the driver for more.
fair process reader = "reader"
begin
D_loop:
    while ~ReaderDone /\ ~(eof /\ ahead = <<>>) do
        \* What came: the arrivals, then the end after them, in one step (the
        \* end is put after its arrivals, so whoever takes it sees them).
        ahead := ahead \o [i \in 1..Len(chunkq) |-> [m |-> chunkq[i], chunk |-> TRUE]];
        chunkq := <<>>;
        if mailbox # "none" then
            if out = "multi" then
                out := "none" || opsOut := opsOut - 1;
            end if;
            if mailbox = "dry" then dry := TRUE; end if;
            if mailbox = "eof" then eof := TRUE; end if;
            mailbox := "none";
        end if;
D_res:
        \* What a pool or lent receive brought.
        if poolRes # 0 then
            if poolRes = End then
                eof := TRUE;
            elsif out = "lent" then
                got := Append(got, poolRes);
                lentGot := lentGot + 1;
            else
                ahead := Append(ahead, [m |-> poolRes, chunk |-> FALSE]);
            end if;
            dry := FALSE;
            out := "none";
            opsOut := opsOut - 1;
            poolRes := 0;
        end if;
D_take:
        if ahead # <<>> then
            \* A read takes the next message; a chunk's buffer goes back.
            got := Append(got, Head(ahead).m);
            if Head(ahead).chunk then returned := returned + 1; end if;
            ahead := Tail(ahead);
        elsif Lent /\ Len(got) > 0 /\ out = "multi" /\ ~stop then
            \* A lent buffer with nothing read ahead: the receive is stopped.
            stop := TRUE;
            batch := 0;
        elsif ~eof /\ (out = "none" \/ Mutation = "arm_twice") then
            \* A lent reader reads plainly first, then into its own buffers.
            if Lent /\ Len(got) > 0 /\ out = "none" then
                out := "lent";
            elsif dry then
                out := "pool";
            else
                \* MUTATION arm_twice: a receive goes out while one is.
                armedWithAhead := armedWithAhead \/ ahead # <<>>;
                out := "multi";
                armed := TRUE;
                stop := FALSE;
            end if;
            opsOut := opsOut + 1;
        else
            \* Waits for the driver.
            await chunkq # <<>> \/ mailbox # "none" \/ poolRes # 0;
        end if;
    end while;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION

(***************************************************************************)
(* The properties.                                                         *)
(***************************************************************************)

\* The reader gets the messages in order, none lost, none twice.
InOrder == got = [i \in 1..Len(got) |-> i]

\* One operation of the side out at a time.
OneOut == opsOut <= 1

\* A receive is never armed with chunks read ahead and not taken.
NoArmWithAhead == ~armedWithAhead

\* A receive holds at most its cap, the arrival that passed it, and what the
\* kernel took before the cancel reached it.
CapHeld == held <= Cap + 1 + Burst

AllDone == \A p \in DOMAIN pc : pc[p] = "Done"

\* Once everything is done, every buffer is back in the group.
BuffersBack == AllDone => free = Total

(***************************************************************************)
(* Witnesses: each claims an interesting case never happens and must be     *)
(* proved wrong.                                                           *)
(***************************************************************************)

\* A receive ends dry, and the next read goes through the pool.
NoDry == dryEnds = 0
\* The ring stops a receive past its cap.
NoStop == stops = 0
\* A message comes straight into a lent buffer.
NoLentGot == lentGot = 0
=============================================================================

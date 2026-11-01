------------------------------ MODULE RingEnd ------------------------------
(***************************************************************************)
(* A stream's end that Linux's multishot receive may never report, and the *)
(* look nuclei's ring makes so the reader still gets it                   *)
(* (`ShardRing::arrived`, `look_for_end` and `submissions_issued` in       *)
(* src/rt/shard/ring/arrivals.rs, `sys::end_queued` in                     *)
(* src/io/sys/unix/linux.rs, `ReadState::submit_ops` and `op_done` in      *)
(* src/io/source/read/ops.rs; the kernel's side is io_recv and             *)
(* io_recv_finish in io_uring/net.c, __io_arm_poll_handler and             *)
(* io_poll_check_events in io_uring/poll.c).                               *)
(*                                                                          *)
(* The story, in plain words. A peer writes a few bytes, then ends its     *)
(* stream (a close, a reset). A reader asks for them with one multishot    *)
(* receive. The kernel takes that receive in three moves: it tries a       *)
(* receive at once; finding nothing, it hangs a poll on the socket (it     *)
(* "arms" it) and looks once more; from then on every change of the        *)
(* socket wakes the poll, and each wake runs a pass that receives again.  *)
(* Between the first try and the arming nothing is listening: bytes and an *)
(* end that come in that gap wake nobody, and only the arming look sees    *)
(* them. That look runs one pass. The pass takes the bytes and asks the    *)
(* socket how much is left; the socket says "nothing", because its count   *)
(* leaves the end out (a Unix stream's every end, a TCP reset). So the     *)
(* pass does not receive again, and waits for a wake. But the end's wake   *)
(* came already, in the gap, and went to nobody. The end sits in the       *)
(* socket for good and the reader waits for good: the hang.               *)
(*                                                                          *)
(* nuclei's ring thread, at the first arrival of a receive, looks at the   *)
(* socket itself (a poll(2) that takes nothing). If the end is queued, it  *)
(* stops the receive; the reader asks again, and a fresh receive's first   *)
(* try finds the end. If no end is queued and the kernel had armed the     *)
(* receive by then, every later end wakes the armed poll, so the look is   *)
(* never needed again. When the kernel's own thread issues the entries     *)
(* (SQPOLL), an arrival may come before the arming: a look then proves     *)
(* nothing, and the next arrival looks again.                             *)
(*                                                                          *)
(* What must never happen: the reader waiting for good with the end in the *)
(* socket; the end reaching the reader before a byte that came before it.  *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC

CONSTANTS
    B,         \* bytes (as pieces) the peer writes before its end
    SqPoll,    \* TRUE: the kernel's own thread issues the entries, so the ring may reap a completion before the arming
    Mutation   \* "none", or the name of the one rule this run takes away

(* --algorithm RingEnd
variables
    sock = 0,           \* pieces in the socket that no receive took yet
    written = 0,        \* pieces the peer wrote
    ended = FALSE,      \* the peer's end is in the socket and no receive took it
    kphase = "none",    \* the receive in the kernel: "none", "queued" (submitted, not tried), "tried", "armed"
    refs = 0,           \* passes the armed poll owes: one per wake, one from an arming look that saw something
    kcancel = FALSE,    \* a cancel of the receive is on its way to the kernel
    cq = <<>>,          \* completions not reaped yet, in order
    out = FALSE,        \* the reader's side counts a receive out
    looked = FALSE,     \* the ring looked at the socket after the arming and found no end, or stopped the receive
    stopping = FALSE,   \* the ring cancelled the receive out
    ahead = 0,          \* pieces handed to the reader's side, not read yet
    got = 0,            \* pieces the reader read
    eof = FALSE,        \* the reader's side saw the end
    stops = 0;          \* (record) receives the ring stopped for an end it found queued

define
    \* The reader has every piece and the end: its work is done.
    ReaderDone == got = B /\ eof
    \* The kernel issued the receive's entry, its arming included, by the time the ring reaps:
    \* always on a ring with no submission thread, else once its poll is armed or it ended.
    Issued == ~SqPoll \/ kphase \in {"armed", "none"}
    \* A completion that hands over `n` pieces and keeps the receive armed.
    Data(n) == [k |-> "data", n |-> n]
    \* The receive's last completion: `why` is "end" or "cancel".
    Final(why) == [k |-> "final", why |-> why]
end define;

\* The peer: writes its pieces one by one, then ends the stream. Each change wakes the poll
\* if it is armed; a change before the arming wakes nobody.
fair process peer = "peer"
begin
P_write:
    \* One piece at a time, so the kernel's moves can fall between them.
    while written < B do
        written := written + 1;      \* the peer wrote one more piece
        sock := sock + 1;            \* it waits in the socket
        if kphase = "armed" then     \* the armed poll hears it
            refs := refs + 1;        \* and owes one more pass
        end if;
    end while;
P_end:
    ended := TRUE;                   \* the end goes into the socket, behind every piece
    if kphase = "armed" then         \* the armed poll hears the end
        refs := refs + 1;            \* and owes a pass for it
    end if;
end process;

\* The kernel: the receive's first try, its arming, the passes the poll owes, and a cancel.
fair process kernel = "kernel"
begin
K_loop:
    while ~ReaderDone do
        either
            \* The first try, once a cancel queued ahead of the entry was taken.
            await kphase = "queued" /\ ~kcancel;
            if sock > 0 then
                cq := Append(cq, Data(sock));        \* it takes every piece there
                sock := 0;                           \* the count says nothing is left
                kphase := "tried";                   \* so it arms instead of trying again
            elsif ended then
                cq := Append(cq, Final("end"));      \* nothing but the end: the receive ends with it
                ended := FALSE;                      \* the end is taken
                kphase := "none";
            else
                kphase := "tried";                   \* nothing at all: it arms
            end if;
        or
            \* The arming: the poll hangs on the socket and looks once. Anything there (pieces,
            \* or the end) runs one pass with no wake behind it.
            await kphase = "tried";
            kphase := "armed";
            refs := IF sock > 0 \/ ended THEN 1 ELSE 0;
        or
            \* A pass the poll owes. It takes every reference owed so far. A second reference
            \* with the end in sight leaves one for another pass; with one alone, no pass follows.
            await kphase = "armed" /\ refs > 0;
            with v = IF refs > 1 /\ ended THEN refs - 1 ELSE refs do
                if sock > 0 then
                    cq := Append(cq, Data(sock));    \* the pass takes every piece
                    sock := 0;                       \* and, the count at nothing, waits
                    refs := refs - v;                \* for what it still owes, if anything
                elsif ended then
                    cq := Append(cq, Final("end"));  \* no piece left and the end: it ends
                    ended := FALSE;
                    kphase := "none";
                    refs := 0;
                else
                    refs := refs - v;                \* nothing to take: it waits
                end if;
            end with;
        or
            \* The ring's cancel reaches the kernel, after the entry it cancels was issued.
            await kcancel /\ kphase # "tried";
            if kphase = "armed" then
                cq := Append(cq, Final("cancel"));   \* the receive ends cancelled
                kphase := "none";
                refs := 0;
            end if;                                  \* else it finds nothing
            kcancel := FALSE;
        end either;
    end while;
end process;

\* The ring's thread: reaps completions in order. On a ring with no submission thread it reaps
\* only once the enter that handed an entry over has issued it.
fair process ring = "ring"
begin
G_loop:
    while ~ReaderDone do
        await cq # <<>> /\ (SqPoll \/ kphase \notin {"queued", "tried"});
        if Head(cq).k = "data" then
            ahead := ahead + Head(cq).n;             \* the pieces go to the side
            \* MUTATION no_look: the ring never looks at the socket.
            if Mutation # "no_look" /\ out /\ ~looked /\ ~stopping then
                if ended then
                    \* The end is queued: the receive is stopped, and the side asks again.
                    stopping := TRUE;
                    kcancel := TRUE;
                    looked := TRUE;
                    stops := stops + 1;
                else
                    \* No end: done for good if the kernel armed the receive already.
                    \* MUTATION look_unissued: a look before the arming counts as done.
                    looked := Issued \/ Mutation = "look_unissued";
                end if;
            end if;
        else
            out := FALSE;                            \* the receive's last completion
            if Head(cq).why = "end" then
                eof := TRUE;                         \* the side holds the end, after its pieces
            end if;                                  \* a cancel: the side asks again when it needs
        end if;
        cq := Tail(cq);
    end while;
end process;

\* The reader: takes what its side holds; with nothing held and no receive out, asks for one.
fair process reader = "reader"
begin
R_loop:
    while ~ReaderDone do
        await ahead > 0 \/ eof \/ ~out;
        if ahead > 0 then
            got := got + ahead;                      \* it reads the pieces, in order
            ahead := 0;
        elsif ~out /\ ~eof then
            out := TRUE;                             \* a new receive goes to the kernel
            looked := FALSE;
            stopping := FALSE;
            kphase := "queued";
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
\* The pieces the completions in `s` carry, added up.
RECURSIVE Carried(_)
Carried(s) == IF s = <<>> THEN 0
              ELSE (IF Head(s).k = "data" THEN Head(s).n ELSE 0) + Carried(Tail(s))
\* No piece is lost or made up: each is in the socket, a completion, the side or the reader.
NoLoss == sock + Carried(cq) + ahead + got = written
\* The end reaches the reader only after every piece the peer wrote before it.
EndLast == eof => written = B /\ got + ahead = B
\* The kernel never holds a receive the reader's side does not count as out.
OneOut == kphase # "none" => out
(***************************************************************************)
(* Witness: claims the case the fix is for never happens, and must be      *)
(* proved wrong.                                                           *)
(***************************************************************************)
\* The ring finds the end queued at an arrival and stops the receive.
NoEndStop == stops = 0
=============================================================================

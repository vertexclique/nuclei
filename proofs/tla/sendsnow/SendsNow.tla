------------------------------ MODULE SendsNow ------------------------------
(***************************************************************************)
(* A stream's owner sending straight from its task's buffer on the one     *)
(* bit the write side's last release left, on a machine whose stores wait  *)
(* in a store buffer, as x86's do (src/io/source/write/calls.rs:          *)
(* `Source::send_as_owner`, which looks at the `WRITE_SENDS` bit of       *)
(* `Source::unsent` under the owner's biased hold, `Side::with_owner`;    *)
(* the write side's `releasing` hook in src/io/source/hooks.rs, which     *)
(* publishes the bit; src/io/source/write.rs: `WriteState::published` and *)
(* `WriteState::sends_now`, what the bit says; src/io/side.rs:            *)
(* `Side::try_biased`, the owner's take, and `Side::take_foreign`).       *)
(*                                                                          *)
(* Who acts. The owner, a task on the thread that runs the stream's home  *)
(* shard, writes two bytes, one write each. A foreign writer, a task on   *)
(* another thread that holds a `Send` handle, makes one owned write of two *)
(* parts (`write_owned`), which must reach the wire whole: nothing between *)
(* its parts. The owner's driver sends what the side holds once the       *)
(* kernel has room. A flusher stands for the memory: the oldest store of  *)
(* the foreign writer's buffer lands at any moment.                        *)
(*                                                                          *)
(* What can happen. Every holder of the write side publishes, as it lets  *)
(* go, whether the side holds nothing a write straight from a task's      *)
(* buffer must come after (`sends`). The owner's write first takes the    *)
(* side biased: a plain look at `intent` and the mode, and while no       *)
(* foreign thread ever came (intent down, not shared), it sends on the    *)
(* bit alone, with no look at the side; else, or when the bit is down or  *)
(* the kernel takes nothing, its poll holds the side and writes or leaves *)
(* the byte behind what the side holds. The foreign writer raises         *)
(* `intent` with a locked instruction and, after its heavy fence, switches *)
(* the side to shared (a plain store) before it takes the word; on a side *)
(* made shared (no heavy fence in the process) it takes the word at once. *)
(* Its write goes out whole, in part (the rest waits in the side), or not *)
(* at all, and its bit sits in its store buffer until a locked            *)
(* instruction or the flusher lands it.                                    *)
(*                                                                          *)
(* What must never happen. A byte of the owner's lands between the parts  *)
(* of the owned write (`Contiguous`), or ahead of a byte it wrote before  *)
(* that waits in the side (`InOrder`). Every byte reaches the wire in the *)
(* end (`AllSent`).                                                        *)
(*                                                                          *)
(* What is abstract. Holds exclude each other (proofs/tla/side and        *)
(* side_biased check that): a hold here is one step. The owner's send on  *)
(* the bit takes no word, so only its looks keep it from a foreign        *)
(* holder: the mode, which the holder stored before its locked take of    *)
(* the word, and on a biased side `intent`, which lands before that. The  *)
(* owner's own stores, and its driver's, are in program order for its own *)
(* looks; a handoff of the shard to another thread runs the heavy fence   *)
(* first (proofs/tla/handoff), so the new thread sees them too. A step   *)
(* that unwinds publishes nothing, as for the idle bit of EdgeSkip.tla.   *)
(* A write is one byte, or the owned write's two parts; the kernel takes  *)
(* a write whole, in part, or not at all.                                  *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC                                             \* numbers, sequences (the wire, the buffer), TLC's helpers

CONSTANTS                                                                    \* what each configuration sets:
    Biased,     \* TRUE: the side starts biased to its owner; FALSE: shared (no heavy fence)
    Foreign,    \* TRUE: the foreign writer makes its owned write
    Mutation    \* "none", or the name of the one rule this run takes away

Owners == 2                                                                  \* the owner's writes, one byte each
Byte(i) == IF i = 1 THEN "b1" ELSE "b2"                                      \* the owner's bytes, in the order written
Total == Owners + (IF Foreign THEN 2 ELSE 0)                                 \* every byte the wire must get

(* --algorithm SendsNow
variables                                                                    \* the shared state:
    wire = <<>>,              \* the bytes the kernel took, in order
    held = <<>>,              \* the bytes the write side holds, to go out in order
    sends = FALSE,            \* the published bit (in memory): the side held nothing at its last release
    intent = FALSE,           \* a foreign thread came, for good (in memory)
    shared = ~Biased,         \* the side takes through its word only (in memory)
    bufW = <<>>,              \* the foreign writer's store buffer, oldest first
    word = "free",            \* who holds the side through its word: "free" or "W"
    fast = FALSE;             \* (record) the owner sent on the bit alone

\* A store of the buffer lands: the mode switch, or the bit.
macro land(s) begin
    if s = "shared" then shared := TRUE;                                     \* the side goes shared
    elsif s = "sends" then sends := TRUE;                                    \* the bit goes up
    else sends := FALSE;                                                     \* or down
    end if;                                                                  \* one store
end macro;

\* A locked instruction of the foreign writer: its buffer lands first (it holds one store at most).
macro drain() begin
    if bufW # <<>> /\ Head(bufW) = "shared" then shared := TRUE;             \* a buffered mode switch lands,
    elsif bufW # <<>> /\ Head(bufW) = "sends" then sends := TRUE;           \* or the bit, up,
    elsif bufW # <<>> then sends := FALSE;                                   \* or down
    end if;                                                                  \* the buffered store is in memory
    bufW := <<>>;                                                            \* and the buffer is empty
end macro;

\* The owner's task: two writes, each on the bit if it may, else through its poll.
fair process owner = "O"                                                     \* weakly fair, like every process
variables b = 1, sent = FALSE;                                               \* the owner's next byte; whether this one went
begin                                                                        \* the owner's steps:
O_write:                                                                     \* the next write
    while b <= Owners do                                                     \* while bytes are left to write
        sent := FALSE;                                                       \* this byte has not gone yet
O_fast:                                                                      \* `send_as_owner`: the biased take, its plain looks, then the bit
        \* The look at `intent` is the biased take's own (side_biased checks it); here a foreign
        \* holder is kept out by the mode, which it stored before its locked take of the word.
        \* MUTATION ignore_shared: the owner sends on the bit though the side is shared.
        \* MUTATION no_bit: the owner sends straight with no look at the bit.
        if ~intent
           /\ (~shared \/ Mutation = "ignore_shared")
           /\ (sends \/ Mutation = "no_bit") then                            \* only its own thread held it, and it held nothing:
            either                                                           \* the kernel takes the byte:
                wire := Append(wire, Byte(b));                               \* it goes out, straight from the task's buffer
                fast := TRUE;                                                \* (record) on the bit alone
                sent := TRUE;                                                \* this byte is done
            or                                                               \* the socket is full:
                skip;                                                        \* the poll's way records it
            end either;                                                      \* the bit's write is answered
        end if;                                                              \* else the poll's way
O_slow:                                                                      \* the poll's way: the side held, the byte written or left behind
        if ~sent then                                                        \* the byte did not go on the bit:
            await word = "free";                                             \* no foreign writer holds the side
            if held = <<>> then                                              \* the side holds nothing:
                either                                                       \* the kernel takes the byte:
                    wire := Append(wire, Byte(b));                           \* it goes out
                or                                                           \* the socket is full:
                    held := Append(held, Byte(b));                           \* it waits in the side
                end either;                                                  \* the write is answered
            else                                                             \* the side holds bytes:
                held := Append(held, Byte(b));                               \* this one goes behind them
            end if;                                                          \* the byte is the side's or the wire's
            \* MUTATION publish_ignores_held: the release says the side holds nothing though it holds bytes.
            sends := (held = <<>>) \/ Mutation = "publish_ignores_held";     \* `releasing`: the owner's own store, the bit
        end if;                                                              \* the hold is let go
        b := b + 1;                                                          \* the next byte
    end while;                                                               \* both written
end process;                                                                 \* end of the owner

\* The foreign writer: one owned write of two parts, from another thread.
fair process writer = "W"                                                    \* weakly fair, like every process
begin                                                                        \* the writer's steps:
W_come:                                                                      \* whether there is a foreign writer
    if ~Foreign then goto Done; end if;                                      \* none in this run
W_intent:                                                                    \* `take_foreign`: intent up, with a locked instruction
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
W_send:                                                                      \* the owned write, both parts at once
    if held = <<>> then                                                      \* the side holds nothing:
        either                                                               \* the kernel takes it whole:
            wire := wire \o <<"p1", "p2">>;                                  \* both parts go out
        or                                                                   \* the kernel takes the first part:
            wire := Append(wire, "p1");                                      \* it goes out,
            held := <<"p2">>;                                                \* and the rest waits in the side
        or                                                                   \* the socket is full:
            held := <<"p1", "p2">>;                                          \* the whole write waits in the side
        end either;                                                          \* the write is answered
    else                                                                     \* the side holds bytes:
        held := held \o <<"p1", "p2">>;                                      \* the write goes behind them
    end if;                                                                  \* the write is the side's or the wire's
W_publish:                                                                   \* `releasing`: the bit, a plain store
    bufW := Append(bufW, IF held = <<>> THEN "sends" ELSE "holds");          \* buffered
W_release:                                                                   \* the word's release, a locked instruction
    drain();                                                                 \* the buffer lands first
    word := "free";                                                          \* the side goes free
end process;                                                                 \* end of the writer

\* The owner's driver: once the kernel has room, what the side holds goes out.
fair process driver = "D"                                                    \* weakly fair, like every process
begin                                                                        \* the driver's steps:
D_loop:                                                                      \* the next room
    while pc["O"] # "Done" \/ pc["W"] # "Done" \/ held # <<>> do             \* for as long as bytes may come
        await (held # <<>> /\ word = "free")                                 \* bytes held and the side free,
              \/ (pc["O"] = "Done" /\ pc["W"] = "Done" /\ held = <<>>);       \* or nothing more will come
        if held # <<>> then                                                  \* bytes held: the driver holds the side
            wire := wire \o held;                                            \* the side's bytes go out, in order
            held := <<>>;                                                    \* the side holds nothing
            sends := TRUE;                                                   \* `releasing`: the owner thread's own store, the bit
        end if;                                                              \* and lets go
    end while;                                                               \* the next room
end process;                                                                 \* end of the driver

\* The memory: the oldest store of the writer's buffer lands at any moment.
fair process flusher = "F"                                                   \* weakly fair, like every process
begin                                                                        \* the flusher's step:
F_land:                                                                      \* the oldest store lands
    while bufW # <<>> \/ pc["W"] # "Done" do                                 \* while stores may come
        await bufW # <<>> \/ pc["W"] = "Done";                               \* one is buffered, or none will be
        if bufW # <<>> then                                                  \* the oldest:
            land(Head(bufW));                                                \* lands in memory
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

\* The owned write's parts are next to each other on the wire: no byte lands between them.
Contiguous == \A i \in 1..(Len(wire) - 1) : wire[i] = "p1" => wire[i + 1] = "p2"   \* p1 is followed by p2

\* The owner's second byte never goes out ahead of its first.
InOrder == \A j \in 1..Len(wire) : wire[j] = "b2" => \E i \in 1..(j - 1) : wire[i] = "b1"   \* b1 came before

\* Every byte reaches the wire in the end.
AllSent == <>(Len(wire) = Total)                                             \* eventually all of them

\* Witness: the owner sends a byte on the bit alone.
NoFast == ~fast                                                              \* claimed never to happen
=============================================================================

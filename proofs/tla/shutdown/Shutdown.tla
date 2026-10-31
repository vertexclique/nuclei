------------------------------ MODULE Shutdown ------------------------------
(***************************************************************************)
(* A stream's close (src/io/source/write.rs: `WriteState::close`, which    *)
(* calls `WriteState::flush` and then `sys::shutdown`; the driver's        *)
(* writes of the write-behind are `WriteState::push_fd`).                  *)
(*                                                                          *)
(* Who acts. A task writes bytes to a stream socket: they wait in the      *)
(* side's write-behind until the driver hands them to the kernel. Then the *)
(* task closes the writing half, again and again. The driver hands the     *)
(* kernel one byte of the write-behind at a time.                         *)
(*                                                                          *)
(* What can happen. A close first waits until the write-behind is out (the *)
(* flush), then asks the kernel to shut the writing half. The first        *)
(* shutdown puts the end of the stream (the FIN) on the wire. The kernel   *)
(* answers a shutdown of a half that is shut already its own way: Linux    *)
(* says it went, macOS says the socket is not connected (ENOTCONN).        *)
(*                                                                          *)
(* What must never happen. The FIN goes ahead of a byte written before the *)
(* close, or a byte is refused because the half was shut under it. A close *)
(* answers something other than what the kernel answered to that shutdown, *)
(* which is what std's shutdown answers for the same call: the code once   *)
(* answered every close after the first from a flag of its own, with Ok,   *)
(* where macOS's kernel says ENOTCONN.                                     *)
(*                                                                          *)
(* What is abstract. The bytes are numbers, written one per step; the      *)
(* kernel takes one per call while the half is open; the peer is the wire. *)
(***************************************************************************)
EXTENDS Integers, Sequences, TLC                                              \* numbers, sequences, TLC's helpers

CONSTANTS                                                                     \* what each configuration sets:
    Bytes,     \* the bytes the task writes before it closes
    Closes,    \* how many times the task closes
    Platform,  \* "linux" or "macos": how the kernel answers a second shutdown
    Mutation   \* "none", or the one rule this run takes away: "own_flag" or "shut_first"

\* The end of the stream on the wire: a number no byte has (the bytes are 1..Bytes), so the wire
\* holds numbers only, which TLC compares.
FIN == 0

\* The kernel's answer to a shutdown of the writing half, given whether it is shut already.
KernelAnswer(isShut) == IF ~isShut THEN "ok"                                  \* an open half shuts: it went
                        ELSE IF Platform = "macos" THEN "ENOTCONN"            \* macOS refuses a second one
                        ELSE "ok"                                             \* Linux says it went again

(* --algorithm Shutdown                                                         \* the algorithm, in PlusCal
variables                                                                     \* the shared state:
    behind = 0,        \* bytes in the write-behind, not handed to the kernel yet
    written = 0,       \* bytes the task wrote
    wire = <<>>,       \* what reached the peer, in order: byte numbers, then FIN
    shut = FALSE,      \* the kernel's writing half is shut
    flag = FALSE,      \* the side's own note that it shut (only the own_flag mutation reads it)
    got = <<>>,        \* what each close answered the task, in order
    kernel = <<>>,     \* what the kernel answered, or would have, to each close, in order
    lost = 0;          \* bytes the kernel refused because the half was shut

\* The task: writes its bytes, then closes the writing half `Closes` times.
fair process task = "task"                                                    \* one task
variables n = 0;                                                              \* the closes it made
begin
T_write:
    \* One byte at a time into the write-behind.
    while written < Bytes do
        \* The write is accepted: its byte waits for the driver.
        written := written + 1;
        \* The write-behind holds it.
        behind := behind + 1;
    end while;
T_close:
    \* Close after close.
    while n < Closes do
        \* The flush: the close waits until the driver handed every byte to the kernel.
        \* MUTATION shut_first: the close does not wait for the write-behind.
        await behind = 0 \/ Mutation = "shut_first";
T_shut:
        \* What the kernel answers this shutdown, asked or not: std's answer for this call.
        kernel := Append(kernel, KernelAnswer(shut));
        if Mutation = "own_flag" /\ flag then
            \* MUTATION own_flag: the side remembers it shut and answers Ok without asking.
            got := Append(got, "ok");
        else
            \* The close asks the kernel and answers what it says.
            got := Append(got, KernelAnswer(shut));
            \* The first shutdown puts the end of the stream on the wire.
            if ~shut then
                \* The FIN goes after whatever the kernel took.
                wire := Append(wire, FIN);
            end if;
            \* The kernel's writing half is shut from now on.
            shut := TRUE;
        end if;
        \* The side notes that it shut.
        flag := TRUE;
        \* One more close made.
        n := n + 1;
    end while;
end process;

\* The driver: hands the write-behind's bytes to the kernel, oldest first, one per call.
fair process driver = "driver"                                                \* the shard's driver
begin
D_loop:
    \* Until every byte the task writes has gone to the kernel.
    while written < Bytes \/ behind > 0 do
        \* It waits for a byte to write.
        await behind > 0;
        if shut then
            \* The writing half is shut: the kernel refuses the byte (EPIPE), and the peer never gets it.
            lost := lost + 1;
        else
            \* The kernel takes the oldest byte of the write-behind and it goes on the wire.
            wire := Append(wire, written - behind + 1);
        end if;
        \* The write-behind holds one byte fewer.
        behind := behind - 1;
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

\* Every close answered what the kernel answered to that shutdown: std's answer for the same call.
AsTheKernel == \A i \in 1..Len(got) : got[i] = kernel[i]

\* The end of the stream comes after every byte written before the close, each once and in
\* order, and nothing comes after it.
FinLast == \A i \in 1..Len(wire) :
              IF wire[i] = FIN THEN i = Bytes + 1                           \* the FIN follows every byte
              ELSE wire[i] = i                                              \* byte i is the i-th thing sent

\* No byte written before the close is refused.
NoneLost == lost = 0

\* `Termination`, which the translation defines: every process ends, so the task makes every
\* close it set out to make and the driver hands every byte on.

(***************************************************************************)
(* Witness: claims an interesting case never happens and must be proved    *)
(* wrong.                                                                  *)
(***************************************************************************)

\* A close is answered ENOTCONN: a second one, on macOS.
NoRefusedClose == \A i \in 1..Len(got) : got[i] # "ENOTCONN"
=============================================================================

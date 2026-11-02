-------------------------------- MODULE Space --------------------------------
(***************************************************************************)
(* The calls on the space of a range of a file on FreeBSD                  *)
(* (src/io/sys/space.rs: the FreeBSD `allocate`, `ends_past_the_file`,     *)
(* `reserve`, `fspacectl`, `deallocate` and `grow`), made by one job of    *)
(* the blocking pool (src/fs/file/space.rs `submit_allocate`, or           *)
(* src/fs/file_ops.rs `allocate` for a file that is not regular) while     *)
(* other parties write the same file, at the grain of one system call.     *)
(*                                                                         *)
(* In plain words: a file holds a few bytes. One call works on the range   *)
(* [Start, End) of it, as its mode says. `Extend` reserves the range with  *)
(* `posix_fallocate`, which grows the file to the range's end. `KeepSize`  *)
(* first looks at the file's length: a range that ends past it is refused  *)
(* (no FreeBSD call reserves space there without growing the file), one    *)
(* inside it is reserved. `PunchHole` makes the range read as zeros with   *)
(* `fspacectl`, which never grows the file; a signal may stop one of its   *)
(* passes part way, and the call goes on with what it left. `ZeroRange`    *)
(* does the same, then looks at the length, and grows a file shorter than  *)
(* the range's end by writing one zero at the range's last byte, through a *)
(* descriptor that does not append (the call's own, or the file opened     *)
(* once more when the call's appends). A file system may reserve nothing   *)
(* (ZFS), a C library may lack `fspacectl` (FreeBSD before 14), a range    *)
(* may be empty, and a step may fail with an I/O error. Meanwhile another  *)
(* party writes bytes, aimed anywhere or appended, and may cut the file.   *)
(*                                                                         *)
(* What must never happen: a byte another party wrote outside the range is *)
(* lost to the call (`NoLostWrite`); the call grows the file past the      *)
(* range's end (`NoGrowthPastEnd`); a `KeepSize` call changes the length   *)
(* (`KeepsLength`); a call that answers an error changed the length        *)
(* (`ErrorKeepsLength`); a call that answered `Ok` leaves a byte from      *)
(* before in the part of the range inside the file (`Zeroed`) or, nobody   *)
(* cutting, a file shorter than the range's end (`Grown`), or did not      *)
(* reserve (`Reserved`). What must happen: the call and every party end    *)
(* (`Termination`, the translation's own).                                 *)
(*                                                                         *)
(* The answers, step by step (the error paths and the code that gives      *)
(* each): "unsupported" at C_keep_check is `allocate`'s own refusal of a   *)
(* `KeepSize` range past the end; "unsupported" at C_free is               *)
(* `deallocate`'s answer when `fspacectl()` found no such call;            *)
(* "eopnotsupp" at C_reserve is the file system's refusal, which           *)
(* `reserve` hands back as `posix_fallocate` returned it (an error number, *)
(* not `errno`), as `Unsupported`; "einval" is the kernel's answer to an   *)
(* empty range, from `posix_fallocate` or `fspacectl`, handed back as is;  *)
(* "error" at C_reserve, C_free and C_grow is any other answer of that     *)
(* call (`EBADF`, `ESPIPE`, `ENOSPC`, `EIO`, the reopen's `EACCES`),       *)
(* handed back as is. A signal before a call changed anything (`EINTR`)    *)
(* makes the same call again (`reserve`'s loop, `retry` in `deallocate`    *)
(* and `grow`): the model's steps are those calls, so a step that was not  *)
(* made is no step. A signal part way through an `fspacectl` pass is the   *)
(* second branch of C_free. A range that ends past the largest offset a    *)
(* file can have never reaches the job: src/fs/file/space.rs `range_end`   *)
(* answers `InvalidInput` before it, and `file_offset` in `allocate` for   *)
(* a file that is not regular.                                             *)
(*                                                                         *)
(* What is abstract: a byte is "old" (there from before), "zero",          *)
(* "theirs" (another party's) or "none" (past the end); each system call   *)
(* is one step, as the kernel makes it under the file's lock; a            *)
(* reservation is a flag, since space is what the call promises and not    *)
(* what any read sees. `posix_fallocate` failing part way leaves what the  *)
(* kernel did and answers its error, as the call made directly would: the  *)
(* model's failed reservation changes nothing, and the length that such a  *)
(* failure may leave is the kernel's.                                      *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Mode,        \* "extend", "keep", "punch" or "zero": the call's `Allocate` mode
    Size0,       \* the file's length when the call starts
    Empty,       \* TRUE: the range is empty (the kernel's `EINVAL`)
    Appends,     \* TRUE: the descriptors append (`O_APPEND`): the other party's writes land at the end
    Cuts,        \* TRUE: another party may cut the file while the call is out
    Reserves,    \* TRUE: the file system reserves space (UFS); FALSE: it refuses (ZFS)
    HasSpacectl, \* TRUE: the C library has `fspacectl` (FreeBSD 14 on)
    Fails,       \* TRUE: a system call of the call may fail with an I/O error
    Mutation     \* "none", or the name of the one rule this run takes away

N == 7                                  \* the positions a file may hold: 0 .. N - 1
Pos == 0 .. (N - 1)                     \* every position
Start == 1                              \* the range starts here
Len == IF Empty THEN 0 ELSE 2           \* the range's length
End == Start + Len                      \* the range ends here (the first position past it)
Max(a, b) == IF a > b THEN a ELSE b     \* the larger of two numbers
Errors == {"unsupported", "eopnotsupp", "einval", "error"}   \* the answers that are errors

(* --algorithm Space
variables
    size = Size0,                       \* the file's length
    cell = [p \in Pos |-> IF p < Size0 THEN "old" ELSE "none"],   \* what each position holds
    answer = "none",                    \* the call's answer: "none" until it ends, "ok", or an error
    reserved = FALSE,                   \* the range's space is reserved
    lost = FALSE,                       \* record: a step of the call took a byte of theirs outside the range
    overgrew = FALSE,                   \* record: a step of the call grew the file past the range's end
    resized = FALSE,                    \* record: a step of the call changed the length
    keepGrew = FALSE;                   \* record: a `KeepSize` call grew the file

define
    \* The call has ended.
    CallDone == pc[1] = "Done"
    \* A byte another party wrote outside the range is never lost to the call.
    NoLostWrite == ~lost
    \* The call never grows the file past the range's end.
    NoGrowthPastEnd == ~overgrew
    \* A `KeepSize` call leaves the length as it found it.
    KeepsLength == ~keepGrew
    \* A hole never changes the length.
    HoleKeepsLength == Mode = "punch" => ~resized
    \* A call that answers an error changed no length.
    ErrorKeepsLength == (CallDone /\ answer \in Errors) => ~resized
    \* A hole or a zeroed range that answered `Ok` left no byte from before inside the range.
    Zeroed == (CallDone /\ answer = "ok" /\ Mode \in {"punch", "zero"})
                  => \A p \in Start .. (End - 1) : p < size => cell[p] # "old"
    \* A growing call that answered `Ok`, with nobody cutting, leaves the file at least to the range's end.
    Grown == (CallDone /\ answer = "ok" /\ Mode \in {"extend", "zero"} /\ ~Cuts) => size >= End
    \* A reservation that answered `Ok` reserved.
    Reserved == (CallDone /\ answer = "ok" /\ Mode \in {"extend", "keep"}) => reserved
    \* Witness: the call can answer an error.
    NoError == ~(CallDone /\ answer \in Errors)
end define;

fair process call = 1                   \* the blocking pool's job of `allocate`
variables
    seen = 0,                           \* the length the call looked at
    rOff = Start,                       \* where what is left of the range starts
    rLen = Len,                         \* how much of the range is left
    signals = 1;                        \* how many times a signal may stop a pass part way
begin
C_start:
    if Mode = "extend" then             \* a reservation goes to the kernel at once
        goto C_reserve;
    elsif Mode = "keep" then            \* one that keeps the length looks at it first
        goto C_look_keep;
    elsif Mode = "zero" /\ Mutation = "grow_first" then   \* the broken order: grow, then zero
        goto C_look;
    else                                \* a hole or a zeroed range: the free comes first
        goto C_free;
    end if;
C_look_keep:
    seen := size;                       \* `ends_past_the_file`: the length, from `fstat`
C_keep_check:
    if End > seen /\ Mutation # "keep_past_end" then   \* the range ends past the file
        answer := "unsupported";        \* refused: no call reserves there and keeps the length
        goto Done;                      \* nothing changed
    end if;
C_reserve:                              \* `reserve`: one `posix_fallocate`
    if Len = 0 then                     \* an empty range
        answer := "einval";             \* the kernel's answer, handed back
    elsif ~Reserves then                \* a file system that reserves nothing
        answer := "eopnotsupp";         \* its answer, handed back as `Unsupported`
    else
        either                          \* the reservation is made
            if End > size then          \* the range ends past the file: the kernel grows it
                if Mode = "keep" then   \* a `KeepSize` call that got here
                    keepGrew := TRUE;   \* changed the length
                end if;
                cell := [p \in Pos |-> IF p >= size /\ p < End THEN "zero" ELSE cell[p]];   \* the new part reads as zeros
                size := End;            \* the file reaches the range's end
                resized := TRUE;        \* the call changed the length
            end if;
            reserved := TRUE;           \* the space is the file's
            answer := "ok";             \* and the call says so
        or                              \* or the kernel fails (no space, an I/O error)
            await Fails;                \* only in the runs that allow it
            answer := "error";          \* its answer, handed back; the model's reservation changed nothing
        end either;
    end if;
    goto Done;                          \* a reservation is one call
C_free:                                 \* `deallocate`: one `fspacectl` pass over what is left
    if ~HasSpacectl then                \* the C library has no `fspacectl`
        answer := "unsupported";        \* refused before any change
        goto Done;
    elsif rLen = 0 then                 \* an empty range
        answer := "einval";             \* the kernel's answer, handed back
        goto Done;
    else
        either                          \* the pass ends: what is left of the range inside the file reads as zeros
            cell := [p \in Pos |-> IF p >= rOff /\ p < rOff + rLen /\ p < size THEN "zero" ELSE cell[p]];
            rOff := rOff + rLen;        \* nothing of the range is left
            rLen := 0;                  \* `fspacectl` says so in the range it hands back
        or                              \* a signal stops the pass after one byte: it answers 0, the rest left
            await signals > 0 /\ rLen > 1;   \* at most once, and with more than one byte left
            signals := signals - 1;     \* the signal came
            cell := [p \in Pos |-> IF p = rOff /\ p < size THEN "zero" ELSE cell[p]];   \* one byte read as zeros
            rOff := rOff + 1;           \* the range handed back starts after it
            rLen := rLen - 1;           \* and is one byte shorter
        or                              \* or the kernel fails (an I/O error)
            await Fails;                \* only in the runs that allow it
            answer := "error";          \* its answer, handed back
            goto Done;                  \* the free never grows the file
        end either;
    end if;
C_more:
    if rLen > 0 /\ Mutation # "one_pass" then   \* the pass left part of the range
        goto C_free;                    \* `deallocate` goes on with it
    elsif Mode = "zero" /\ Mutation # "grow_first" then   \* a zeroed range grows next
        goto C_look;
    else                                \* a hole, or the broken order's last step
        answer := "ok";                 \* the call is done
        goto Done;
    end if;
C_look:
    seen := size;                       \* `grow`: the length, from `fstat`
C_grow:
    if seen < End then                  \* the file is shorter than the range
        if Mutation = "truncate" then   \* the broken growth: `ftruncate` to the range's end
            lost := lost \/ \E p \in Pos : p >= End /\ cell[p] = "theirs";   \* it cuts what lies past the end
            cell := [p \in Pos |-> IF p >= End THEN "none" ELSE IF p >= size THEN "zero" ELSE cell[p]];
            size := End;                \* the length is the range's end, whatever it was
            resized := TRUE;            \* the call changed the length
        else
            either                      \* one zero at the range's last byte
                with w = (IF Appends /\ Mutation = "aimed_appends" THEN size ELSE End - 1) do   \* where it lands: the broken one through the appending descriptor
                    lost := lost \/ (cell[w] = "theirs" /\ (w < Start \/ w >= End));   \* a byte of theirs outside the range written over
                    overgrew := overgrew \/ w >= End;   \* a zero past the range's end
                    cell := [p \in Pos |-> IF p = w THEN "zero" ELSE IF p >= size /\ p < w THEN "zero" ELSE cell[p]];   \* the gap before it reads as zeros
                    resized := resized \/ w >= size;   \* the call changed the length if the zero lies past the end
                    size := Max(size, w + 1);   \* a write past the end grows the file
                end with;
            or                          \* or the write, or the reopen it needs, fails
                await Fails;            \* only in the runs that allow it
                answer := "error";      \* its answer, handed back: the range reads as zeros, the length is as it was
                goto Done;
            end either;
        end if;
    end if;
C_after:
    if Mutation = "grow_first" then     \* the broken order zeroes after it grew
        goto C_free;
    else                                \* the growth is the last step
        answer := "ok";
        goto Done;
    end if;
end process;

fair process writer = 2                 \* another party, writing the file through its own descriptor
variables writes = 2;                   \* how many bytes it writes
begin
W_write:
    while writes > 0 do                 \* each a write of one byte, at any moment
        with w \in (IF Appends THEN {size} ELSE Pos) do   \* appended at the end, or aimed anywhere
            await w < N;                \* the model's file has room for it
            cell := [p \in Pos |-> IF p = w THEN "theirs" ELSE IF p >= size /\ p < w THEN "zero" ELSE cell[p]];   \* its byte, the gap before it zeros
            size := Max(size, w + 1);   \* a write past the end grows the file
        end with;
        writes := writes - 1;           \* one fewer to write
    end while;
end process;

fair process cutter = 3                 \* another party that may cut the file once
begin
X_cut:
    if Cuts then                        \* only in the runs that allow it
        with c \in 0 .. size do         \* to any length up to the one it has (the same length cuts nothing)
            cell := [p \in Pos |-> IF p >= c THEN "none" ELSE cell[p]];   \* what lay past the cut is gone
            size := c;                  \* the new length
        end with;
    end if;
end process;
end algorithm; *)
=============================================================================

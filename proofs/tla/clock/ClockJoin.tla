------------------------------- MODULE ClockJoin -------------------------------
(***************************************************************************)
(* A paused clock's jump against a shard that joins the runtime's guests   *)
(* while the jump is decided (src/rt/paused.rs: `attempt`, its             *)
(* `clock.members()` before and after; src/time/clock.rs:                  *)
(* `Clock::members`, `Clock::member_changed`), at the grain of each look. *)
(*                                                                          *)
(* In plain words: an attempt reads the member count, then looks at every  *)
(* member shard (each must be idle, its earliest timer noted), then looks  *)
(* again: the member count unchanged, and every shard it saw still idle    *)
(* with no new park. Only then does it move the clock to the earliest     *)
(* timer it saw. A guest shard (a `block_on` thread's own shard, say)     *)
(* joins: it counts itself in, which bumps the member count, while it      *)
(* runs; it then arms a timer one unit from the clock and parks, idle.    *)
(*                                                                          *)
(* Who acts: the attempt, and the guest that joins. What must never        *)
(* happen: the clock moved past an armed timer of any member.              *)
(*                                                                          *)
(* What is abstract: the member shards that were there all along are one  *)
(* idle shard with a timer at Far; the guest parks once; the jobs count is *)
(* zero (Clock.tla checks it, with the parks and the wake of the shards    *)
(* that came due). The second look and the jump are one step here: a      *)
(* timer armed from another thread between the two is the open race of a  *)
(* foreign arm against the jump (the sync audit's item 8), which this     *)
(* model leaves out.                                                       *)
(***************************************************************************)
EXTENDS Integers, TLC

CONSTANTS
    Near,      \* how far from the clock the joining guest arms its timer
    Far,       \* the timer of the shard that was there all along
    Mutation   \* "none", or the name of the one rule this run takes away

AttemptT == "attempt"
GuestT == "guest"

(* --algorithm ClockJoin
variables
    clock = 0,              \* the paused clock
    members = 1,            \* the member count (`Clock::members`)
    guestIn = FALSE,        \* the guest counts as a member
    guestIdle = FALSE,      \* the guest parked, its timer armed
    guestAt = 0;            \* the guest's timer

define
    \* No armed timer of a member is ever behind the clock.
    ClockNeverPassesArmed ==
        /\ clock <= Far
        /\ guestIdle => clock <= guestAt
end define;

fair process attempt = AttemptT
variables seenMembers = 0, earliest = Far, sawGuest = FALSE;
begin
A_members:
    \* The member count, read first.
    seenMembers := members;
A_look:
    \* The first look: every member idle (a member that runs ends the attempt); the
    \* earliest timer noted.
    if guestIn /\ ~guestIdle then
        goto Done;
    else
        sawGuest := guestIn;
        earliest := IF guestIn /\ guestAt < Far THEN guestAt ELSE Far;
    end if;
A_relook:
    \* The second look: the count unchanged, and every shard seen still idle; then the jump.
    \* MUTATION no_members_recheck: the count is not read again.
    if (members # seenMembers /\ Mutation # "no_members_recheck")
       \/ (sawGuest /\ ~guestIdle) then
        goto Done;                 \* a shard joined or woke: no jump this time
    else
        clock := IF earliest > clock THEN earliest ELSE clock;
    end if;
end process;

fair process guest = GuestT
begin
G_join:
    \* A shard joins the guests: counted in, while it runs.
    guestIn := TRUE;
    members := members + 1;
G_arm:
    \* It arms a timer Near units from the clock, and parks.
    guestAt := clock + Near;
    guestIdle := TRUE;
end process;
end algorithm; *)
\* The PlusCal translator writes the TLA+ form of the algorithm between the next two lines.
\* proofs/tla/run_tlc.sh runs it on a temporary copy before every check, so the tree never
\* holds a translation that could fall out of date with the algorithm above.
\* BEGIN TRANSLATION
\* END TRANSLATION
=============================================================================

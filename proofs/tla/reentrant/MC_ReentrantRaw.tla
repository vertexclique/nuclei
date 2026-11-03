---------------------------- MODULE MC_ReentrantRaw ----------------------------
(***************************************************************************)
(* The programs the reentrant-against-handoff configurations run.          *)
(***************************************************************************)
EXTENDS ReentrantRaw

\* A read, a nested read, and both let go.
NestedRead == <<"R", "R", "r", "r">>
\* A read dropped while it may be queued, then a read and its release.
DropThenRead == <<"D", "R", "r">>
=============================================================================

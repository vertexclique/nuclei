-------------------------- MODULE MC_ReentrantRwLock --------------------------
(***************************************************************************)
(* The model the configurations check: the reentrant reader-writer lock   *)
(* with its constants set by each configuration (`.cfg`), which says in    *)
(* its first lines what it is about.                                       *)
(***************************************************************************)
EXTENDS ReentrantRwLock

\* A writer that reads inside and lets the write go first (a downgrade); a nested reader.
DowngradeNested == <<<<"W", "R", "w", "r">>, <<"R", "R", "r", "r">>>>
\* A writer that reads inside and lets the read go first; a reader refused the write.
ReadInWriteRefused == <<<<"W", "R", "r", "w">>, <<"R", "W", "w", "r">>>>
\* Two owners that each read, let go and read again: claims race for a freed record.
ClaimRace == <<<<"R", "r", "R", "r">>, <<"R", "r", "R", "r">>>>
\* An owner that joins a write and a read beside one that writes; and one that joins two
\* reads beside it.
JoinWriteRead == <<<<"JWR", "r", "w">>, <<"W", "w">>>>
JoinReads == <<<<"JRR", "r", "r">>, <<"W", "w">>>>
\* An owner whose write waits while it reads, beside one that reads.
WriteQueuedRead == <<<<"WQR", "r">>, <<"R", "r">>>>
=============================================================================

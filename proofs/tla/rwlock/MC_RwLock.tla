------------------------------ MODULE MC_RwLock ------------------------------
(***************************************************************************)
(* The model the configurations check: the reader-writer lock with its    *)
(* constants set by each configuration (`.cfg`), which says in its first   *)
(* lines what it is about. An operation: "read", "write", "upg" (an        *)
(* upgradable read), "upgrade" (an upgradable read upgraded to the write), *)
(* "downgrade" (a write downgraded to a read), "drop_read" and              *)
(* "drop_write" (dropped at the first wait); each is released at its end.  *)
(***************************************************************************)
EXTENDS RwLock

ReadWrite == <<<<"read", "read">>, <<"write">>>>
WriteWrite == <<<<"write">>, <<"write", "write">>>>
UpgradeRead == <<<<"upgrade">>, <<"read">>>>
DowngradeRead == <<<<"downgrade">>, <<"read">>>>
UpgUpg == <<<<"upg">>, <<"upg">>>>
DropWrite == <<<<"read">>, <<"drop_write">>, <<"read">>>>
DropWriteTwo == <<<<"read">>, <<"drop_write">>>>
BargeWriter == <<<<"read", "read", "read">>, <<"write">>>>
\* An upgradable read takes the lock, a writer tries it.
UpgWrite == <<<<"upg">>, <<"write">>>>
\* A write downgraded and held until a reader joins it, against that reader.
DowngradeHoldRead == <<<<"downgrade_hold">>, <<"read">>>>
\* An upgrade against a task that reads, then writes: the write queues behind the upgrade.
UpgradeReadWrite == <<<<"upgrade">>, <<"read", "write">>>>
\* An upgrade dropped while it waits for a reader, against that reader and one more read.
UpgradeDropRead == <<<<"upgrade_drop">>, <<"read", "read">>>>
\* An upgradable read dropped while it waits, against an upgradable read and a reader.
DropUpg == <<<<"upg">>, <<"drop_upg">>, <<"read">>>>
\* Two readers.
ReadRead == <<<<"read">>, <<"read">>>>
=============================================================================

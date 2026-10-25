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
=============================================================================

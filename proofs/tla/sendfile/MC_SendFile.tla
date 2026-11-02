------------------------------ MODULE MC_SendFile ------------------------------
(***************************************************************************)
(* The model the send_file configurations check: a chunk the blocking pool *)
(* sends and the stream's next write, with the constants set by each        *)
(* configuration (`.cfg`), which says in its first lines what it is about.  *)
(***************************************************************************)
EXTENDS SendFile
=============================================================================

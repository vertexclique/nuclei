--------------------------- MODULE MC_CallerMode ---------------------------
(***************************************************************************)
(* The model the caller-mode read configurations check: a peer, the ring  *)
(* and a task that reads through the handle, then makes its own calls,    *)
(* with their constants set by each configuration (`.cfg`).               *)
(***************************************************************************)
EXTENDS CallerMode
=============================================================================

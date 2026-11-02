------------------------------ MODULE MC_Claim ------------------------------
(***************************************************************************)
(* The model the Claim configurations check: the threads that poll one    *)
(* biased side and which of them takes it as its owner, with the rule     *)
(* each configuration (`.cfg`) takes away, said in its first lines.        *)
(***************************************************************************)
EXTENDS Claim                                                                \* the model, as it is
=============================================================================

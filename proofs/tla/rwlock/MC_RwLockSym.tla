---------------------------- MODULE MC_RwLockSym ----------------------------
(***************************************************************************)
(* The model the configurations that swap tasks check: the reader-writer  *)
(* lock of MC_RwLock, its tasks named by model values (t1, t2, t3) rather  *)
(* than by number, so that a symmetry set can swap two tasks that run the  *)
(* same program. Each configuration (`.cfg`) says in its first lines what  *)
(* it is about.                                                            *)
(***************************************************************************)
EXTENDS MC_RwLock

\* Three names for tasks, model values a symmetry set may swap.
CONSTANTS t1, t2, t3

\* DropWrite, its tasks named: t1 and t3 read, t2's write is dropped.
DropWriteNamed == (t1 :> <<"read">>) @@ (t2 :> <<"drop_write">>) @@ (t3 :> <<"read">>)

\* Every way to rename the two readers of DropWriteNamed between themselves. A
\* configuration that names it under SYMMETRY checks a state and its renaming
\* once: sound here, as both run the same program from the same start, and no
\* property singles one task out (each reads every task, or every node, alike).
ReadersSwapped == Permutations({t1, t3})
=============================================================================

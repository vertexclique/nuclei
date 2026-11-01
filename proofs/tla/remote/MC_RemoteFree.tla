--------------------------- MODULE MC_RemoteFree ---------------------------
(***************************************************************************)
(* The model the remote free configurations check: two owners of one      *)
(* record, one after the other, and threads that free their blocks, with  *)
(* their constants set by each configuration (`.cfg`), which says in its  *)
(* first lines what it is about.                                          *)
(***************************************************************************)
EXTENDS RemoteFree

\* Every way to rename the blocks among themselves, and the freeing threads among themselves. A
\* configuration that names it under SYMMETRY checks a state and its renamings once: sound here,
\* as every block starts with the system and an owner takes any one there, every freeing thread
\* runs the same code on the blocks it was handed, and no property singles one block or one
\* thread out. The owners are not renamed: "o" holds the record first and "n" after it.
Perms == Permutations(Blocks) \cup Permutations(Foreign)
=============================================================================

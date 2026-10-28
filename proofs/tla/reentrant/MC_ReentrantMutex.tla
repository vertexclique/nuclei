--------------------------- MODULE MC_ReentrantMutex ---------------------------
(***************************************************************************)
(* The model the configurations check: the reentrant mutex with its       *)
(* constants set by each configuration (`.cfg`), which says in its first   *)
(* lines what it is about.                                                 *)
(***************************************************************************)
EXTENDS ReentrantMutex

\* Each owner locks two deep, lets one go, locks again, and lets all go.
TwoDeep == <<<<"L", "L", "U", "L", "U", "U">>, <<"L", "L", "U", "L", "U", "U">>>>
\* An owner three deep beside one that locks once, twice over.
DeepShallow == <<<<"L", "L", "L", "U", "U", "U">>, <<"L", "U", "L", "U">>>>
=============================================================================

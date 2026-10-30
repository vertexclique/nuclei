---------------------------- MODULE MC_SideBiased ----------------------------
(***************************************************************************)
(* The model the configurations check: the side guard biased to its owner, *)
(* with its constants set by each configuration (`.cfg`), which says in    *)
(* its first lines what it is about. A configuration file cannot write a   *)
(* sequence, so the lists of calls a foreign thread makes are named here   *)
(* and given to `Calls` and `OldCalls` with `<-`.                          *)
(***************************************************************************)
EXTENDS SideBiased                                                           \* the model, as it is

NoCalls == <<>>                                                              \* no call
OnePoll == <<"poll">>                                                        \* one poll
TwoPolls == <<"poll", "poll">>                                               \* two polls, one after the other
OneDefer == <<"defer">>                                                      \* one defer (a buffer given back, say)
OneReq == <<"req">>                                                          \* one request (a dropped handle's close, say)
DeferReq == <<"defer", "req">>                                               \* a defer, then a request
PollDefer == <<"poll", "defer">>                                             \* a poll, then a defer
PollReq == <<"poll", "req">>                                                 \* a poll, then a request
=============================================================================

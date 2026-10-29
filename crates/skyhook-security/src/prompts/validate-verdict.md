Record the verdict with `research` `verdict`. Validate only when the mechanism, reachable
preconditions and stated consequence survive review. Reject with `disproved` when the mechanism
does not hold, or `policy_excluded` when the scope's recorded policy keeps a real finding out of
reports; a duplicate is merged, not rejected, naming in `keep` the duplicate's premises the
survivor still needs. A variant of another scenario of the same root flaw is not a duplicate:
review it on its own; its root's group decides whether it owes a demonstration. When the
scenario fails as formulated but its
flaws support another reading — a different consequence, a combination instead of a sequence,
another path — raise that reading as its own candidate, or add a follow-up (`research` `add_unit`)
naming it when you cannot formulate it. Validated means you have no open question that could
change the verdict. A doubt that would need a follow-up about this candidate is an `unproven`
verdict: first record the open question as a premise gating the scenario, instead of
validating and questioning the candidate afterwards. The store plans one follow-up settling
each open condition per revision of the condition, and the candidate returns to review as soon
as its follow-ups end, settled or not. A candidate that returns unproven with conditions still
open has had them followed up and gets no more, and this review ends in a verdict: decide each
such condition — an empirical one by a bounded harness, one about the deployment or the
attacker by the scope's facts as above, one about the mechanism by the evidence — then validate,
reject, or record `unproven` only while a condition you asked waits on the user's answer.
Another unproven verdict is refused unless something new is open to wait on.
Follow-ups you plan are only for other candidates or alternative formulations. For each member flaw, name the other
consequences it could have beyond this scenario, and raise each as its own candidate or as a
follow-up naming the flaw; the verdict covers only the scenario you were given. Record
`unproven` too when a dependency or context you cannot reach blocks a decision, with a premise
gating the scenario that names the exact unresolved check.

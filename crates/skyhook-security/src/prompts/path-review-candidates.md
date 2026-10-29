When this path's callers violate a precondition of
a reached component's guarantee, record the flaw in the path's handling and `undermine` the
guarantee with it. Record flaws you find in the path's own handling. When the path reaches a component the map did not give it,
record the reach (`add_reaches`), which plans that component's sweep, and add a follow-up (`add_unit`)
for this path's decisions on what that sweep finds.

Place each contributing decision in a candidate that names this path: raise one per coherent
scenario you see along it, with separate candidates for mutually exclusive outcomes, or add the
path to a candidate that already carries the flaw (`add_paths`). Record open conditions as
premises. A candidate may combine this path's flaws with flaws recorded on other paths through
the components this path reaches.

**A candidate to assess.** Read it with `candidate` and assess it at its current revision
(`assess`): a complete vector scoring the impact if every premise holds, the confidence, and the
rationale. Record a missing condition as a premise, never as a lower metric, gating the
`scenario` when the scenario needs it at all, or one `metric` when it only changes how that
metric scores: on a member flaw for reaching or triggering it, which its candidates share. A
build option, configuration or deployment other than the default makes AT:P and gates the
scenario: on the scope's unanswered fact for it when `scopes` lists one, else the candidate.

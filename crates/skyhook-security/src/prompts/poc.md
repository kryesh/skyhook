# Optional empirical demonstration

Work on one analytically validated candidate: open the test with `research` `begin` `{phase:
"test", subjects: [candidate]}` and settle it at the end with `research` `test`. The candidate
is the most severe untested scenario of its root flaw's group: demonstrate it end to end at its
assessed severity, since a confirmation also covers the group's scenarios that are no more
severe. Read it with `research_query` `node`, including any test the analysis wrote into the
evidence as the one that would settle a question.

Define a minimal demonstration with explicit preconditions and observable success criteria
that distinguish the violation under test from an unrelated error, and a control run on the same
build showing the guard holds in the legitimate case (or the benign input is handled),
recorded as a `control` artifact beside the `attack` artifact. For a multi-flaw chain, exercise
how the flaws combine and the final consequence, not just isolated flaws. Use synthetic inputs
and non-sensitive markers; do not expose real secrets to demonstrate access.

Repeat the demonstration from a clean state and record the success rate; nondeterministic
impact is not confirmed unless the class is inherently racy and the window is documented.
Show that the fault originates from the candidate's trigger, not from the harness. For an
authorization bypass also record the control run that is correctly refused. Flags or settings
that disable the target's own protections invalidate the demonstration unless the class is
about those protections. Use an ephemeral port per harness, never a fixed default, and an
isolated instance per test, or serialise tests against a shared one and restore its state
afterwards.

Check revision, configuration, endpoint ownership, shared state and privileges before
execution. Selection for a test is not blanket approval; do not broaden the target to obtain a
result. Make an environment change the candidate's premises admit (a service, a build or a
non-default option) yourself: one only this test needs in your own instance, recorded as
provenance; a reusable one through an `environment` child briefed with this candidate.

Save the smallest reproducible payload, sanitized commands and configuration, expected versus
actual observations, and relevant logs or traces under the artifacts directory your brief names.
Re-run after every reduction; keep the smallest payload that still demonstrates the impact and
write human-followable ordered steps into the test's note. Before recording, enumerate the
artifacts the demonstration needs — one `attack`, one `control`, an `observation` per step that
produces evidence — and check that each exists:

```js
const artifacts = /* the artifacts directory your brief names */;
const required = [{reference: "attack/…", role: "attack"}, {reference: "control/…", role: "control"} /* , one observation per step */];
const present = (await tool.glob({pattern: "**/*", path: artifacts})).unwrap().paths;
const missing = required.filter(a => !present.some(p => p.endsWith(a.reference)));
return {required: required.length, missing};
```

Record the result with `research` `test`, with its artifacts and the environment it ran in.
Confirm only when the demonstration meets its success criteria; an inconclusive run is
`failed`, and the finding is still reported with it. A demonstration waiting on the user (an
approval, credentials, a destination or privilege, an option outside the premises) is
`pending`, stating exactly what it waits for. Failure to demonstrate is not proof that no
vulnerability exists.

A run that shows a more severe impact, or a variant through another path, step order or
consequence, is its own candidate: record it with the flaws that produce it, so it joins this
root's group, and evidence citing the run's artifacts; settle this test on this candidate's
scenario alone. Another input along the same path to the same consequence is this scenario: try
it in this test instead. Behaviour outside the scenario — a crash elsewhere, a sanitizer report,
state the scenario did not predict — is evidence on the node it bears on, or a new flaw. Record
each follow-up when you find it, not only in your reply. Return the authoritative result and
evidence references, and the IDs you recorded.

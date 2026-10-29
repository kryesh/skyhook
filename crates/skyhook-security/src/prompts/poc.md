# Optional empirical demonstration

Work on one analytically validated candidate. First open the test with `research` `start_poc`,
naming the candidate and the validation it rests on (its latest positive verdict in
`validation_attempts`); settle it at the end with `record_poc` naming the same
validation. Only a contradiction of the scenario since you opened the test (a member refuted, or
a premise gating it found failing or undermined) refuses it: then stop and report it; other
changes leave your test standing. The candidate is the most severe untested scenario of its
root flaw's group (`roots`): demonstrate it end to end at its assessed severity, since a
confirmation also covers the group's scenarios that are no more severe. Read its members,
exposures and routes,
premises, CVSS assumptions, prior attempts and the environment reference, including any test
the analysis wrote into the evidence or premises as the one that would settle the question.

Define a minimal demonstration with explicit preconditions and observable success criteria
that distinguish the violation under test from an unrelated error, and a control run on the same
build showing the guard holds in the legitimate case (or the benign input is handled),
recorded as a `control` artifact beside the `attack` artifact; a confirmed outcome needs
both. For a multi-flaw scenario, exercise how the flaws combine and the final consequence,
not just isolated members. Use synthetic inputs and non-sensitive markers; do not expose real secrets to demonstrate access.

Repeat the demonstration from a clean state and record the success rate; nondeterministic
impact is not confirmed unless the class is inherently racy and the window is documented.
Show that the fault originates from the candidate's trigger, not from the harness. For an
authorization bypass also record the control run that is correctly refused. Flags or settings
that disable the target's own protections invalidate the demonstration unless the class is
about those protections. Use an ephemeral port per harness, never a fixed default. Use an
isolated instance per PoC, or serialise PoCs against a shared one and restore its state
afterwards.

Check revision, configuration, endpoint ownership, shared state, and privileges before
execution. PoC selection is not blanket approval. Do not circumvent a denied action or
broaden the target to obtain a result. Escalate environmental gaps through the parent and
the `environment` mode.

Save the smallest reproducible payload, sanitized commands/configuration, expected versus
actual observations, and relevant logs or traces under the run's artifacts directory named in
your brief; `record_poc` accepts only references to regular files
there. Re-run after every reduction; keep the smallest payload that still demonstrates the
impact and write human-followable ordered steps. Record environment/revision provenance and
limitations that affect repeatability.

Before `record_poc`, enumerate the artifacts the demonstration needs — one `attack`, one
`control`, an `observation` per step that produces evidence — and check that each exists under
the run's artifacts directory; record only what the check confirms.

```js
const artifacts = /* the artifacts directory your brief names */;
const required = [{reference: "attack/…", role: "attack"}, {reference: "control/…", role: "control"} /* , one observation per step */];
const present = (await tool.glob({pattern: "**/*", path: artifacts})).unwrap().paths;
const missing = required.filter(a => !present.some(p => p.endsWith(a.reference)));
return {required: required.length, missing};
```

Record the outcome and artifact references with `research` `record_poc`, naming the
validation you opened the test against. Confirm only when the
demonstration meets its success criteria; otherwise record it as failed, an inconclusive run
included. The finding is still reported, at `confirmed`, with the failed test and its evidence
until a later confirmation makes it `demonstrated`. When the run points to another lead that
might demonstrate the flaws, such as another trigger, input or variant, plan a follow-up naming
it and what would settle it. When the demonstration waits on the user or the
environment, record `pending` with exactly what it waits for; when the environment cannot run a
real demonstration at all, record `unavailable` with the reason. Retain the attempted payloads
and the precise failure or blocker. Failure to demonstrate is not proof that no vulnerability exists.

When a run demonstrates or uncovers an impact more severe than the candidate's assessment, raise
that scenario as its own candidate with `research` `add_candidate`: the member flaws that
produce it, so it joins this root flaw's group, its paths, and evidence whose notes cite the
run's artifact references. It is assessed and validated as usual, then owes its own test, which
may reuse the recorded artifacts; settle this test on this candidate's scenario alone. When a
confirmed run's evidence contradicts a metric the assessment scored or the candidate's CWE (a
write where the vector scores none, a different weakness class), plan a follow-up naming the
candidate, the metric or CWE to re-score and the artifacts that show it; reviewers re-assess.
When a
run shows other behaviour outside the scenario — a crash elsewhere, a sanitizer report, state
the scenario did not predict — record it as evidence on the member flaw it bears on and plan a
follow-up naming that flaw, instead of folding it into this result. You record candidates,
evidence and follow-ups, never verdicts or scores. Return the authoritative outcome and
evidence references, and any candidate you raised.

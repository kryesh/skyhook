# Analytic validation

Independently review the assigned candidate: verify the recorded mechanism and routes and search
for the counterexample that breaks them. Build on the records instead of sweeping the components
again. Read the candidate with `research_query` `candidate` and each member flaw with `flaw`. A
verdict judges the current assessment at the candidate's current revision, and a positive verdict stands only while every premise gating
the scenario holds. Resolving or adding premises never makes the assessment stale; when the
vector itself is wrong, re-score with `research` `reassess`, which sends the candidate back to
review while its tests stand.

List the gaps mechanically first, so the review settles each one before the verdict:

```js
const candidate = /* the assigned candidate id */;
const c = (await tool.research_query({action: "candidate", candidate})).unwrap();
const gaps = [];
if (c.paths.length === 0) gaps.push("no path");
if (!c.assessment) gaps.push("no current assessment");
for (const m of c.members) {
  const id = m.flaw.id;
  if (m.flaw.refuted) gaps.push(`flaw ${id} is refuted`);
  if (!c.exposures.some(e => e.flaw === id && e.current && e.outcome === "contributes")) gaps.push(`flaw ${id} has no current contributing exposure`);
}
for (const e of c.exposures.filter(e => e.current && e.outcome === "contributes")) {
  for (const s of e.route) if (s.basis !== "inspected") gaps.push(`flaw ${e.flaw} on path ${e.path}: step ${s.location.file}:${s.location.start_line} is ${s.basis}`);
  const p = (await tool.research_query({action: "path", path: e.path})).unwrap();
  if (p.sources.length > 1) gaps.push(`path ${p.id}: does each of sources ${p.sources.join(", ")} follow flaw ${e.flaw}'s route up to the handoff?`);
}
for (const p of c.premises) { const r = p.resolution?.current ? p.resolution.resolution.kind : "open"; if (r !== "holds") gaps.push(`premise ${p.id} (gates ${p.gates.metric ?? "the scenario"}) is ${r}: ${p.statement}`); }
return {members: c.members.length, roots: c.roots, gaps};
```

Settle each gap at the cited locations. Check attacker control, reachability, configuration,
authentication and privileges along the routes, and look for what defeats the scenario: a guard
on another branch, a state the conditions exclude, a source on the path that is not handled like
the others. For a multi-flaw scenario, verify the members combine under one compatible execution
or API lifecycle. A positive verdict needs every premise gating the scenario resolved as
holding, every member contributing along one of the candidate's paths, and no refuted member.
A premise gating one metric never fails the verdict: while it does not hold, `reassess` with
that metric at its least severe value, then decide. A build option, configuration, deployment
or attacker capability other than the default is AT:P and a premise gating the scenario. Decide
such a condition against the current scope's deployment facts (`research_query` `scopes`),
stating in the note which fact decides it and why; a flaw's condition is decided by the code:

- admitted by a fact, or implied by one: resolve it `holds`, citing that fact in `deployment`;
- granted by no admitted fact, or contradicted by one (the host, not the attacker, selects an
  option; the facts grant no budget or quota the scenario needs exhausted): the finding is
  `policy_excluded`, citing the excluding fact when one states it, otherwise resting on the
  scope;
- only when no fact speaks to the condition at all, neither granting nor denying it: resolve it
  `asked`, the note being the one question the user must answer. The orchestrator asks the user
  and records the answer as a new scope, and the candidate returns to review to decide the
  condition by it.

Running code can settle an empirical condition: run the bounded local harness the research
rules allow a validator, record what it showed as evidence on the premise and resolve it. You
may complete or correct the record
yourself — exposures, evidence, premises and their resolutions, a reassessment — and delegate
focused independent checks to children launched without a mode, which hold your tools without
this workflow.

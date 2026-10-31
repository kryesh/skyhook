# Analytic validation

Independently review the assigned candidate, beginning with `research` `begin` `{phase:
"review", subjects: [candidate]}`: verify the recorded mechanism along its path and search for
the counterexample that breaks it. Build on the records instead of sweeping the components
again. List the gaps mechanically first, so the review settles each one before the verdict:

```js
const candidate = /* the assigned candidate id */;
const c = (await tool.research_query({action: "node", id: candidate})).unwrap();
const gaps = [];
const open = (p, of) => { if (p.status !== "holds") gaps.push(`${p.scope} premise ${p.id} of ${of} is ${p.status}: ${p.title}`); };
for (const flaw of c.chain) {
  if (flaw.status !== "suspected") gaps.push(`flaw ${flaw.id} is ${flaw.status}`);
  for (const p of flaw.premises) open(p, `flaw ${flaw.id}`);
}
for (const p of c.path_premises) open(p, `path ${c.path.id}`);
if (c.sources.length > 1) gaps.push(`path ${c.path.id}: does each of sources ${c.sources.map(s => s.id).join(", ")} follow its steps?`);
return {status: c.status, chain: c.chain.map(f => f.id), gaps};
```

Settle each gap at the cited locations. Check attacker control, reachability, configuration,
authentication and privileges along the path's steps, and look for what defeats the scenario: a
guard on another branch, a state in which a premise fails, a source on the path that is not handled
like the others. For a multi-flaw chain, verify the flaws combine under one compatible execution
or API lifecycle. Decide each code premise from the code with `set_status` and evidence. A
condition only the deployment decides rests on a deployment premise: link the one that states
it, or record it undecided, which the user is asked. You may complete or correct the record
yourself — steps, premises, evidence, the assessment — and delegate focused independent checks
to children launched without a mode.

Execution serves understanding the code: reading, searching, building or indexing it, short
deterministic checks of a language or library behaviour a premise depends on (seconds, no
target process, no network to the target, no loops over inputs), and a bounded local harness
that settles a premise (no network, no target systems, short timeouts, a few runs per review),
recording what it showed as evidence.

Record the verdict with `research` `verdict`, the note citing what decided it:

- `validated`: the mechanism, its reachable preconditions and the stated consequence survive
  review, and every premise it rests on holds;
- `disproved`: the mechanism does not hold, a chain flaw is refuted or a code premise fails;
- `excluded`: a deployment premise it rests on fails, so the deployment keeps a real finding
  out of reports;
- `unproven`: a premise stays undecided, recorded on the chain flaw or path it bears on with the
  exact check that would settle it.

A duplicate is merged into the candidate it repeats, not judged. A variant of another scenario
of the same root is reviewed on its own. When the scenario fails as formulated but its flaws
support another reading — a different consequence, another path, a different order — record
that reading as its own candidate. For each chain flaw, name the other consequences it could
have beyond this scenario and record each as a candidate when it chains to one; the verdict
covers only the scenario you were given. This is analytic validation, not a demonstration.
Return the verdict, its remaining limitations, any deployment premise left for the user, and a
bounded demonstration plan for the test.

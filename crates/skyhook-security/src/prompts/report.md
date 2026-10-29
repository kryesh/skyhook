# One finding per root flaw

Populate the assigned root flaw's finding from authoritative research state. Every candidate in
the root's group (its flaws trace back to the root through `enables` relations) holding a
positive verdict belongs to it: the headline is the most severe demonstrated scenario, otherwise
the most severe, and the finding's severity is the highest among them; the others are its
variants. Use `research_query` and the available environment records to reconcile each
scenario's current verdict, flaws, exposures and routes, premises, assessment, and demonstration
history. Each scenario is reported at its confidence: `confirmed` by review (untested, noted
because a demonstrated scenario at least as severe covers it, or with a failed test, stated with
its evidence) or `demonstrated` by a confirmed test. A rejected scenario is not reported. You
never change verdicts or scores: when a scenario's scoring looks inconsistent with its evidence,
write it as recorded and state the inconsistency, with the scenario and the evidence, in your
return.

Record the finding with `finding_report` `populate`. The facts come from the records: the
scenarios, CVSS vectors and scores, verdicts, premises, flaws, routes, paths, sources, tests and
their artifacts are stated from the store, never from your text. You write the prose around
them, as it stands now, never narrating what changed since an earlier finding:

- `title`: the defect and its consequence in one line, in the library's own terms — name the
  code and the behavior, not this review's vocabulary or its record ids.
- `summary`: the paragraph the maintainer reads first: who must control what for this to matter
  and what they get from it, the configuration it needs, the affected versions, then the vector
  and score. A reader who stops here knows whether their deployment is exposed and what to do
  about it.
- `analysis`: the root cause at file and line, then a walkthrough in prose: the input an
  attacker supplies, where it travels, which check should have stopped it and why it does not,
  and the harm that follows — step by step, for a reader who has never opened these files. Name
  the defect site and the trust boundary crossed (who supplies the input, whose process pays)
  rather than restating every route step the rendered scenario lists.
- `impact`: who is harmed and how, each premise the severity rests on named before the severity
  it prices, so a maintainer reads the impact under its conditions rather than a severity
  qualified afterwards; then the impact's limitations.
- `remediation`: a suggested fix first — the concrete change at file and line, written against
  the project's own constraints (its minimum runtime versions, its documented behavior), with
  an alternative shape when the primary is invasive — then the regression tests that pin the
  defect, the verification steps naming the recorded harness, and interim mitigations for
  deployments that cannot upgrade.
- `notes`: one `{candidate, note}` per scenario: why it matters and what it needs beyond the
  headline's conditions, and what distinguishes it from its sibling scenarios, so a maintainer
  does not read one defect twice.
- `narratives`: one `{poc, narrative}` per stated test: what the run showed, a failed one
  included, and what limits its repeatability. A demonstration that stood a platform in for the
  target's (a POSIX host carrying Windows path semantics) says so, and what a run on the real
  platform would settle.
- `reproduction`: prerequisites, the headline's steps and its expected or observed outcome, and
  cleanup instructions. Label unexecuted steps as proposed, not reproduced. A step names the
  recorded artifacts it uses in `artifacts`, each a PoC ID and the artifact's recorded reference
  (as `poc_attempts` lists it); never write a file path for an artifact.
- `sections`: only material the outline above has no place for, such as a disclosure timeline or
  an affected-versions table, each a one-line heading and a body. A threat-model section belongs
  here whenever the scenarios rest on premises beyond the library's default configuration.

Headings you write are set below the report's own, and raw HTML shows as written. Use
placeholders or approved secret references in commands and configuration. Do not run a new PoC.
Populating unchanged scenarios again replaces the finding's write-up.

When the root already has a finding, read it with `research_query` `finding`: the scenarios it
published and its write-up, in the shape `populate` takes. A stale one owes this report: revise
that write-up against the scenarios as they stand rather than starting over, keeping what still
holds, rewriting what changed evidence contradicts, and dropping notes, narratives and artifact
citations of scenarios or tests no longer stated.

Before `populate`, assemble the evidence and check the write-up mechanically: the group's
scenarios with their standing, each one's PoC attempts with artifacts by role, the previous
finding, and each field with content; a missing field or note is a gap to resolve, not to
paper over.

Read the write-up back as its first reader: the library's maintainer, who did not sit in this
review, owes its findings nothing, and will read `summary`, `analysis`, `impact` and
`remediation` in that order and stop when their question is answered. Where they would ask — is
my deployment exposed, which versions, does this reproduce on my platform, what do you want me
to change, what does the attacker already control — the write-up answers it or names it open
rather than papering over it.

```js
const root = /* the assigned root flaw id */;
const drain = async call => { const out = []; for (let after = 0;;) { const p = (await call({after, limit: 100})).unwrap(); out.push(...p.items); if (p.next_after == null) return out; after = p.next_after; } };
const group = await drain(page => tool.research_query({action: "candidates", filter: {root}, page}));
const scenarios = group.filter(c => ["validated", "noted", "reported"].includes(c.status));
const tests = {};
for (const c of scenarios) for (const a of await drain(page => tool.research_query({action: "poc_attempts", candidate: c.id, page}))) tests[a.id] = {candidate: c.id, result: a.result, artifacts: a.artifacts.map(f => ({poc: a.id, reference: f.reference, role: f.role}))};
const prior = await tool.research_query({action: "finding", root}); // fails when the root has no finding
const previous = prior.state === "failed" ? null : prior.unwrap();
const report = previous?.write_up ?? {root, title: "", summary: "", analysis: "", impact: "", remediation: "", notes: [], narratives: [], sections: [], reproduction: {prerequisites: [], steps: [], outcome: /* … */ null, cleanup: []}};
const empty = Object.entries(report).filter(([, v]) => typeof v === "string" && !v.trim()).map(([k]) => k);
if (report.reproduction.steps.length === 0) empty.push("reproduction.steps");
for (const c of scenarios) if (!report.notes.some(n => n.candidate === c.id)) empty.push(`notes: ${c.id}`);
return {scenarios: scenarios.map(c => ({id: c.id, status: c.status, confidence: c.confidence, severity: c.severity})), prior: previous && {id: previous.finding.id, status: previous.finding.status}, tests, empty};
```

A finding states each scenario's current test; write a narrative for each of those. Finalize
through `finding_report` `populate`, read the finding back with `research_query` `finding` and
check its write-up is what you intended, then return the stored finding identity, its status,
evidence level, unresolved limitations, and any scoring inconsistency you found.

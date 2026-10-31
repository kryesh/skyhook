# One report per root flaw

Begin with `research` `begin` `{phase: "report", subjects: [root]}`, then write the assigned root
flaw's report from the research as it stands. Every candidate of the root's group that stands
validated or noted is one of its scenarios; the report renders a table of them with their
paths, chains, vectors, scores, statuses, confidence and the test each stands on from the store,
so you write the prose around it. Read the scenarios and the previous report:

```js
const root = /* the assigned root flaw id */;
const drain = async query => { const out = []; for (let after = 0;;) { const p = (await tool.research_query({action: "list", ...query, page: {after, limit: 100}})).unwrap(); out.push(...p.items); if (p.next_after == null) return out; after = p.next_after; } };
const scenarios = [];
for (const c of (await drain({kind: "candidate", root})).filter(c => ["validated", "noted"].includes(c.status))) {
  const node = (await tool.research_query({action: "node", id: c.id})).unwrap();
  const test = node.tests.find(t => t.id === node.settling_test);
  scenarios.push({id: c.id, status: c.status, confidence: node.confidence, severity: node.assessment?.severity, test: test && {id: test.id, result: test.result, artifacts: test.artifacts}});
}
const flaw = (await tool.research_query({action: "node", id: root})).unwrap();
const previous = flaw.finding == null ? null : (await tool.research_query({action: "node", id: flaw.finding})).unwrap();
return {scenarios, previous: previous && {id: previous.id, current: previous.current, title: previous.title, body: previous.body}};
```

Read each scenario with `node` as you write. Record the report with `research` `report`: a
`title` naming the defect and its consequence in the library's own terms, and a markdown `body`
with these sections:

- **Summary**: who must control what for this to matter and what they get from it, the
  configuration it needs, the affected versions, then the headline's vector and score.
- **Analysis**: the root cause at file and line, then a walkthrough: the input an attacker
  supplies, where it travels, which check should have stopped it and why it does not, and the
  harm that follows, for a reader who has never opened these files.
- **Impact**: who is harmed and how, each premise the severity rests on named before the
  severity it prices; then the limitations.
- **Remediation**: the concrete change at file and line, written against the project's own
  constraints, with an alternative when the primary is invasive; the regression tests that pin
  the defect; interim mitigations.
- **Reproduction**: prerequisites, steps and the observed outcome, citing test ids and their
  artifacts by path; unexecuted steps labelled proposed.
- A section per other scenario: what it needs beyond the headline's conditions and what
  distinguishes it, so a maintainer does not read one defect twice.

Cite node and test ids as written. Do not run a new test. When a scenario's scoring looks
inconsistent with its evidence, write it as recorded and say so in your return. A previous
report is revised against the scenarios as they stand rather than started over: keep what
still holds, rewrite what changed evidence contradicts.

Read the report back as its first reader: the library's maintainer, who did not sit in this
review and will read Summary, Analysis, Impact and Remediation in that order, stopping when
their question is answered. Where they would ask — is my deployment exposed, which versions,
does this reproduce on my platform, what do you want me to change — the report answers it or
names it open. Return the finding id, its scenarios' confidence, unresolved limitations, and
any scoring inconsistency you found.

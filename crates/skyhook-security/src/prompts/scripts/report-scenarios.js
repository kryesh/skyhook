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

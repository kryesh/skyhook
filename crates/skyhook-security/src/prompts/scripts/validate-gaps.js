const candidate = /* the assigned candidate id */;
const c = (await tool.research_query({action: "node", id: candidate})).unwrap();
const gaps = [], on = new Set(c.actions.flatMap(a => a.steps.map(s => s.component.id)));
const open = (p, of) => { if (p.status !== "holds") gaps.push(`${p.scope} premise ${p.id} of ${of} is ${p.status}: ${p.title}`); };
for (const flaw of c.chain) {
  if (flaw.status !== "suspected") gaps.push(`flaw ${flaw.id} is ${flaw.status}`);
  if (!on.has((await tool.research_query({action: "node", id: flaw.id})).unwrap().component.id)) gaps.push(`flaw ${flaw.id} lies off every action's steps: a missing step or action?`);
  for (const p of flaw.premises) open(p, `flaw ${flaw.id}`);
}
for (const a of new Map(c.actions.map(a => [a.path.id, a])).values()) {
  for (const p of a.premises) open(p, `path ${a.path.id}`);
  if (a.sources.length > 1) gaps.push(`path ${a.path.id}: does each of sources ${a.sources.map(s => s.id).join(", ")} follow its steps?`);
}
return {status: c.status, chain: c.chain.map(f => f.id), gaps};

const candidates = [/* the assigned candidate ids */];
const reviews = [];
for (const candidate of candidates) {
  const c = (await tool.research_node({id: candidate})).unwrap();
  const gaps = [], on = new Set(c.actions.flatMap(a => a.steps.map(s => s.component.id)));
  const open = (p, of) => { if (p.status !== "holds") gaps.push(`${p.scope} premise ${p.id} of ${of} is ${p.status}: ${p.title}`); };
  for (const flaw of c.chain) {
    if (flaw.status !== "suspected") gaps.push(`flaw ${flaw.id} is ${flaw.status}`);
    if (!on.has((await tool.research_node({id: flaw.id})).unwrap().component.id)) gaps.push(`flaw ${flaw.id} lies off every action's steps: a missing step or action?`);
    for (const p of flaw.premises) open(p, `flaw ${flaw.id}`);
  }
  for (const a of new Map(c.actions.map(a => [a.path.id, a])).values()) {
    for (const p of a.premises) open(p, `path ${a.path.id}`);
    const inputs = [...new Set(a.states.filter(s => s.scope === "input").map(s => s.id))];
    if (inputs.length > 1) gaps.push(`path ${a.path.id}: which of input states ${inputs.join(", ")} does the chain need, at what trust?`);
    for (const id of new Set(a.states.filter(s => s.scope === "stored" && s.access === "read").map(s => s.id))) gaps.push(`path ${a.path.id} reads stored state ${id}: does the chain need it, and which path writes it with the least privilege?`);
  }
  reviews.push({candidate, status: c.status, chain: c.chain.map(f => f.id), gaps});
}
return reviews;

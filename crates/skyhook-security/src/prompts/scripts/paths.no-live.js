const workers = /* how many inputs to sweep at once */, inputs = [/* ids of the map's components that receive external input */];
const drain = async kind => { const out = []; for (let after = 0;;) { const p = (await tool.research_query({action: "list", kind, page: {after, limit: 100}})).unwrap(); out.push(...p.items); if (p.next_after == null) return out; after = p.next_after; } };
const briefs = [], interfaces = [], served = new Set();
for (const id of inputs) { const c = (await tool.research_query({action: "node", id})).unwrap(); briefs.push(...c.files.map(f => `Record every entry point in ${f}.`)); interfaces.push(...c.interfaces); }
const jobs = (await tool.jobs({all: true, limit: 1})).unwrap();
let launches = jobs.jobs.length + (jobs.older ?? 0);
const outcomes = [];
for await (const {index, value, error} of new WorkPool(workers).map(briefs, async prompt => {
  const child = await tool.agent({mode: "sources", depth: 1, name: `sources-${++launches}`, prompt});
  return {prompt, child: child.id, state: child.state, error: child.error};
})) outcomes[index] = value ?? {error};
// Paths sharing a first step are one handoff to reconcile; a path without steps is unmatched.
const handoffs = {}, unmatched = [];
for (const p of await drain("path")) {
  const first = (await tool.research_query({action: "node", id: p.id})).unwrap().steps[0];
  if (!first) { unmatched.push(p.id); continue; }
  const at = first.location ?? null;
  (handoffs[JSON.stringify([first.component.id, at])] ??= {component: first.component.id, at, paths: []}).paths.push(p.id);
}
return {inputs: briefs.length, unserved: interfaces.filter(i => !served.has(i)), handoffs: Object.values(handoffs), unmatched, outcomes};

# One component

Your parent assigned you one sweep unit; `research_query` `unit` names its component. Sweep the
component once, whatever reaches it: `research_query` `dossier` lists its files and anything
already recorded. Measure it first:

```js
const unit = /* your unit id */;
const {component} = (await tool.research_query({action: "unit", unit})).unwrap();
const dossier = (await tool.research_query({action: "dossier", component})).unwrap();
const dirs = new Map();
for (const f of dossier.files) { const i = f.lastIndexOf("/"); const dir = i < 0 ? "." : f.slice(0, i); if (!dirs.has(dir)) dirs.set(dir, new Set()); dirs.get(dir).add(f.slice(i + 1)); }
const sizes = [];
for (const [dir, names] of dirs) for (const e of (await tool.read({path: dir})).unwrap().entries.files) if (names.has(e.name)) sizes.push({file: dir === "." ? e.name : `${dir}/${e.name}`, bytes: e.bytes});
return {component, files: sizes.length, bytes: sizes.reduce((sum, f) => sum + f.bytes, 0), largest: sizes.sort((a, b) => b.bytes - a.bytes).slice(0, 10)};
```

Sweep it yourself when you can read and reason over all of it in one context. Otherwise split it
into parts by call family or responsibility (a state machine, a parser stage, one interface and
its helpers), not by file boundaries alone, so that each part is a coherent piece one agent can
hold, and sweep the parts with `component-part` children:

```js
const workers = /* how many parts to sweep at once */;
const component = /* the component id */;
const parts = [/* one line per part: what it covers and where */];
const jobs = (await tool.jobs({all: true, limit: 1})).unwrap();
let launches = jobs.jobs.length + (jobs.older ?? 0);
const outcomes = [];
for await (const {index, value, error} of new WorkPool(workers).map(parts, async part => {
  const child = await tool.agent({mode: "component-part", depth: 1, name: `component-part-${component}-${++launches}`, prompt: `Component ${component}, part: ${part}.`});
  return {part, child: child.id, state: child.state, result: child.result?.reply, error: child.error};
})) outcomes[index] = value ?? {error};
return {parts: parts.length, outcomes};
```

With every part's outcome in hand, read the dossier again and reconcile across parts: check each
part's preconditions against the other parts' guarantees, follow each part's misreporting flaw
into the other parts' consumers, and record the guarantees, flaws, checks and undermines links
that only show across parts. A contract whose producer and consumer lie in different parts is
yours: trace it end to end, with evidence at each producing site, or check it. A part whose child failed is swept again or deferred, never
dropped.

End with `research` `outcome` on your unit: completed once the sweep, and when split every part
and the reconciliation, is done (a completed sweep covers the files the component held when you
started; files added later owe another sweep), or blocked, inconclusive or deferred with the
limitation. Do not wait for further instructions. Return the IDs you and your parts recorded and
anything no one could settle.

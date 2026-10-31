# One path

Your parent assigned you one path. Begin with `research` `begin` `{phase: "sweep", subjects:
[path]}`, read it with `research_query` `node`, and sweep it once, whatever else reaches its
components. Measure the files its steps enter first:

```js
const path = /* your path id */;
const node = (await tool.research_query({action: "node", id: path})).unwrap();
const files = new Set();
for (const step of node.steps) for (const f of (await tool.research_query({action: "node", id: step.component.id})).unwrap().files) files.add(f);
const dirs = new Map();
for (const f of files) { const i = f.lastIndexOf("/"); const dir = i < 0 ? "." : f.slice(0, i); if (!dirs.has(dir)) dirs.set(dir, new Set()); dirs.get(dir).add(f.slice(i + 1)); }
const sizes = [];
for (const [dir, names] of dirs) for (const e of (await tool.read({path: dir})).unwrap().entries.files) if (names.has(e.name)) sizes.push({file: dir === "." ? e.name : `${dir}/${e.name}`, bytes: e.bytes});
return {steps: node.steps.length, files: sizes.length, bytes: sizes.reduce((sum, f) => sum + f.bytes, 0), largest: sizes.sort((a, b) => b.bytes - a.bytes).slice(0, 10)};
```

Sweep it yourself when you can read and reason over all of it in one context. Otherwise split
it into stretches of consecutive steps, each a coherent piece one agent can hold, and sweep
them with `sweep-part` children:

```js
const workers = /* how many stretches to sweep at once */;
const path = /* the path id */;
const parts = [/* one line per stretch: its steps and what they cover */];
const jobs = (await tool.jobs({all: true, limit: 1})).unwrap();
let launches = jobs.jobs.length + (jobs.older ?? 0);
const outcomes = [];
for await (const {index, value, error} of new WorkPool(workers).map(parts, async part => {
  const child = await tool.agent({mode: "sweep-part", depth: 1, name: `sweep-part-${path}-${++launches}`, prompt: `Path ${path}, stretch: ${part}.`});
  return {part, child: child.id, state: child.state, result: child.result?.reply, error: child.error};
})) outcomes[index] = value ?? {error};
return {parts: parts.length, outcomes};
```

With every stretch's outcome in hand, read the path again and reconcile across stretches: check
whether other stretches' flaws meet each stretch's premises, follow each stretch's misreported
value into the next one's consumers, and record the flaws, premises and candidates that only
show across stretches. A stretch whose child failed is swept again, never dropped.

Once the whole path is swept, mark it with `research` `set_status` `{path, status: "swept",
note}`; a sweep you cannot finish leaves it unswept, the reason in your reply. Do not wait for
further instructions. Return the IDs you and your stretches recorded and anything no one could
settle.

# Sources grouped into paths

Within the component map, record the entry points an actor can drive and group them into paths,
so each distinct flow of input is swept once.

A source is one concrete entry point — one route, event or message handler, read site,
callback — not a trust class or ingress family: recording "all authenticated requests" as one
source hides every path behind it. Consider network input, stdin or another process's output,
files, environment variables, IPC, arguments, plugins, deserialization, queues and previously
stored attacker-controlled data; a trusted-looking file or upstream service is not necessarily a
trusted origin. Do not filter sources because they look low impact.

Sweep every file of the components the map says receive external input, one `sources-file` child
per file; each records its sources, each on the existing path its flow matches or on a path of
its own whose first step is its handoff into implementation:

```js
const workers = /* how many files to sweep at once */, inputs = [/* ids of the map's components that receive external input */];
const drain = async kind => { const out = []; for (let after = 0;;) { const p = (await tool.research_query({action: "list", kind, page: {after, limit: 100}})).unwrap(); out.push(...p.items); if (p.next_after == null) return out; after = p.next_after; } };
const files = [];
for (const id of inputs) files.push(...(await tool.research_query({action: "node", id})).unwrap().files);
const jobs = (await tool.jobs({all: true, limit: 1})).unwrap();
let launches = jobs.jobs.length + (jobs.older ?? 0);
const outcomes = [];
for await (const {index, value, error} of new WorkPool(workers).map(files, async file => {
  const child = await tool.agent({mode: "sources-file", depth: 1, name: `sources-file-${++launches}`, prompt: `Record every entry point in ${file}.`});
  return {file, child: child.id, state: child.state, error: child.error};
})) outcomes[index] = value ?? {error};
// Paths sharing a first step are one handoff to reconcile; a path without steps is unmatched.
const handoffs = {}, unmatched = [];
for (const p of await drain("path")) {
  const first = (await tool.research_query({action: "node", id: p.id})).unwrap().steps[0];
  if (!first) { unmatched.push(p.id); continue; }
  const at = first.location ?? null;
  (handoffs[JSON.stringify([first.component.id, at?.file, at?.start_line])] ??= {component: first.component.id, at, paths: []}).paths.push(p.id);
}
return {files: files.length, handoffs: Object.values(handoffs), unmatched, outcomes};
```

Reconcile each handoff group into the paths it is: sources interpreted identically up to the
handoff — the same parsing, the same state touched, the same call into the same component —
share one path, so link the group's sources to one of its paths with `path_source`, unlink them
from the others, link each other path's `path_premise`s to the kept path, and then move any
candidate on another to it; a path left without sources is not swept. Sources that differ in
trust or handling before the handoff keep separate paths, and when in doubt, keep them apart.
Join groups that are one handoff described differently. Then extend each path past its handoff
with `link` `path_step`: each component the handoff's component hands the input to, in order,
with its location and how. Place each unmatched path's handoff yourself.
Every source belongs to a path, and every file has an outcome.

Return the path IDs, the files that had no entry points, and what you could not resolve, such as
dynamic registration or generated dispatch. An enumerated inventory is not proven exhaustive
coverage.

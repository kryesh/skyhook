# Sources grouped into paths

Within the recorded scope and its component map, record the entry points an actor can drive and
group them into paths, so each distinct way input is interpreted is reviewed once.

A source is one concrete entry point — one route, event or message handler, read site,
callback — not a trust class or ingress family: recording "all authenticated requests" as one
source hides every path behind it. Consider network input, stdin or another process's output,
files, environment variables, IPC, arguments, plugins, deserialization, queues and previously
stored attacker-controlled data; a trusted-looking file or upstream service is not necessarily a
trusted origin. Do not filter sources because they look low impact.

A path is the sources interpreted identically up to their handoff into implementation: the same
parsing, the same state touched, the same call into the same component. Sources that differ in
trust, kind, parsing or state before the handoff belong to separate paths; when in doubt,
separate them.

Sweep every file of the components the map says receive external input, one `sources-file`
child per file:

```js
const workers = /* how many files to sweep at once */, inputs = [/* ids of the map's components that receive external input */];
const drain = async call => { const out = []; for (let after = 0;;) { const p = (await call({after, limit: 100})).unwrap(); out.push(...p.items); if (p.next_after == null) return out; after = p.next_after; } };
const files = [];
for (const id of inputs) files.push(...(await tool.research_query({action: "dossier", component: id})).unwrap().files);
const jobs = (await tool.jobs({all: true, limit: 1})).unwrap();
let launches = jobs.jobs.length + (jobs.older ?? 0);
const outcomes = [];
for await (const {index, value, error} of new WorkPool(workers).map(files, async file => {
  const child = await tool.agent({mode: "sources-file", depth: 1, name: `sources-file-${++launches}`, prompt: `Record every entry point in ${file}.`});
  return {file, child: child.id, state: child.state, error: child.error};
})) outcomes[index] = value ?? {error};
// Sources sharing a handoff are one group to reconcile; one without a handoff is unmatched.
const handoffs = {}, unmatched = [];
for (const s of await drain(page => tool.research_query({action: "sources", page}))) {
  if (!s.handoff) { unmatched.push(s.id); continue; }
  (handoffs[JSON.stringify([s.handoff.at.file, s.handoff.calls])] ??= {at: s.handoff.at, calls: s.handoff.calls, component: s.handoff.component ?? null, sources: []}).sources.push(s.id);
}
return {files: files.length, handoffs: Object.values(handoffs), unmatched, outcomes};
```

Reconcile the groups, and place each unmatched source or handoff without a component yourself:
split a group whose sources differ in trust, kind or handling before the handoff (`research_query`
`sources`), and join groups that are one handoff described differently. Record each path with
`research` `add_path`: a description of the shared interpretation, its sources, and `reaches`
from the map — the component that owns the handoff and the components its description says it
hands work to, each with how. Do not trace beyond the handoff; path review refines reach. Every
source belongs to a path, and every file has an outcome.

Return the path IDs, the files that had no entry points, and what you could not resolve, such as
dynamic registration or generated dispatch. An enumerated inventory is not proven exhaustive
coverage.

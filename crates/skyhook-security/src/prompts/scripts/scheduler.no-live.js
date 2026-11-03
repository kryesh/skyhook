const workers = /* from the research state */, artifacts = /* from the research state */, source = /* your workspace */;
let stopped = false, held = false;
const jobs = (await tool.jobs({all: true, limit: 1})).unwrap();
let launches = jobs.jobs.length + (jobs.older ?? 0);
const tries = new Map(), spent = new Map(), tried = (phase, e) => `${phase}:${e.node}:${e.basis}`;
const keys = () => [...spent.values()].map(({replies, ...key}) => key), nodes = ({entries}) => entries.map(e => e.node);
const MODES = {sweep: ["sweep", 2, "Sweep path"], probe: ["probe", 2, "Probe path", true], check: ["check", 2, "Check state"], settle: ["question", 1, "Settle premise", true], chain: ["question", 1, "Chain flaw", true], assess: ["assess", 1, "Assess candidate"], review: ["validate", 2, "Review candidate", true], report: ["report", 1, "Report root flaw"]};
const next = async (running, free) => {
  if (stopped) return [];
  const busy = [...running.values()].flatMap(nodes), ready = [];
  held = false;
  for (let after = null; ready.length < free;) {
    const page = (await tool.research_query({action: "startable", running: busy, spent: keys(), after, limit: Math.min(free - ready.length, 1000)})).unwrap();
    held ||= page.held;
    for (const head of page.items) {
      const item = {key: head.node, phase: head.phase, entries: [head, ...(head.bundle ?? [])]};
      busy.push(...nodes(item));
      ready.push(item);
    }
    if (page.next == null) break;
    after = page.next;
  }
  return ready;
};
for await (const r of new WorkPool(workers).feed(next, async ({key, phase, entries}) => {
  const [mode, depth, brief, runs] = MODES[phase], name = `${mode}-${key}-${++launches}`, workspace = `${artifacts}/${name}`;
  return await tool.agent({mode, depth, name, ...runs && {workspace}, prompt: `${brief}${entries.length > 1 ? "s" : ""} ${nodes({entries}).join(", ")}.${runs ? ` Your workspace is your working directory, ${workspace}; the target is in ${source}.` : ""} Your reply is kept with your task, but agents work from the records: record what you found, ruled out or need answered, a question as an undecided premise.`});
}, {wake: receive})) {
  stopped ||= !!r.wake?.stop;
  const {phase, entries = []} = r.item ?? {};
  for (const e of entries) {
    const replies = [...tries.get(tried(phase, e)) ?? [], r.error ?? r.value.error ?? r.value.result?.reply];
    tries.set(tried(phase, e), replies);
    if (replies.length > 1) spent.set(`${phase}:${e.node}`, {phase, node: e.node, basis: e.basis, replies});
  }
}
const synthesis = (await tool.research_query({action: "synthesis", spent: keys()})).unwrap();
for (const e of synthesis.exhausted) e.reply ??= spent.get(`${e.phase}:${e.node}`)?.replies.at(-1);
return {synthesis, held: held && !stopped};

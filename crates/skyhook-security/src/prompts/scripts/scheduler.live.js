const workers = /* from the research state */, artifacts = /* from the research state */;
const approved = [/* under ask: candidates the user approved */], unblocked = [/* pending test ids whose blocker the user resolved */];
let stopped = false, held = false;
const jobs = (await tool.jobs({all: true, limit: 1})).unwrap();
let launches = jobs.jobs.length + (jobs.older ?? 0);
const tries = new Map(), spent = new Map(), tried = (phase, e) => `${phase}:${e.node}:${e.basis}`;
const keys = () => [...spent.values()].map(({replies, ...key}) => key);
const MODES = {sweep: ["sweep", 2, "Sweep path"], probe: ["probe", 2, "Probe path", true], settle: ["question", 1, "Settle premise", true], chain: ["question", 1, "Chain flaw", true], assess: ["assess", 1, "Assess candidate"], review: ["validate", 2, "Review candidate", true], report: ["report", 1, "Report root flaw"]};
MODES.test = ["poc", 2, "Demonstrate candidate", true];
const next = async (running, free) => {
  if (stopped) return [];
  const busy = [...running.values()].flatMap(r => r.entries.map(e => e.node)), ready = [];
  held = false;
  for (let after = null; ready.length < free;) {
    const page = (await tool.research_query({action: "startable", running: busy, spent: keys(), live: {approved, unblocked}, after, limit: Math.min(free - ready.length, 1000)})).unwrap();
    held ||= page.held;
    for (const head of page.items) {
      const entries = [head, ...(head.bundle ?? [])];
      busy.push(...entries.map(e => e.node));
      ready.push({phase: head.phase, key: head.node, entries});
    }
    if (page.next == null) break;
    after = page.next;
  }
  return ready;
};
for await (const r of new WorkPool(workers).feed(next, async ({phase, key, entries}) => {
  const [mode, depth, brief, runs] = MODES[phase], name = `${mode}-${key}-${++launches}`;
  return await tool.agent({mode, depth, name, prompt: `${brief}${entries.length > 1 ? "s" : ""} ${entries.map(e => `${e.node} (${e.title})`).join(", ")}.${runs ? ` Working directory: ${name} in ${artifacts}.` : ""} Your reply is kept with your task, but agents work from the records: record what you found, ruled out or need answered, a question as an undecided premise.`});
}, {wake: receive})) {
  stopped ||= !!r.wake?.stop;
  for (const e of r.item?.entries ?? []) {
    const replies = [...tries.get(tried(r.item.phase, e)) ?? [], r.error ?? r.value.error ?? r.value.result?.reply];
    tries.set(tried(r.item.phase, e), replies);
    if (replies.length > 1) spent.set(`${r.item.phase}:${e.node}`, {phase: r.item.phase, node: e.node, basis: e.basis, replies});
  }
}
const synthesis = (await tool.research_query({action: "synthesis", spent: keys()})).unwrap();
for (const e of synthesis.exhausted) e.reply ??= spent.get(`${e.phase}:${e.node}`)?.replies.at(-1);
return {synthesis, held: held && !stopped};

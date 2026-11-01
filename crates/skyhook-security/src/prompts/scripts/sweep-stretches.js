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

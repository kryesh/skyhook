const path = /* your path id */;
const node = (await tool.research_node({id: path})).unwrap();
const files = new Set();
for (const step of node.steps) for (const f of (await tool.research_node({id: step.component.id})).unwrap().files) files.add(f);
const dirs = new Map();
for (const f of files) { const i = f.lastIndexOf("/"); const dir = i < 0 ? "." : f.slice(0, i); if (!dirs.has(dir)) dirs.set(dir, new Set()); dirs.get(dir).add(f.slice(i + 1)); }
const sizes = [];
for (const [dir, names] of dirs) for (const e of (await tool.read({path: dir})).unwrap().entries.files) if (names.has(e.name)) sizes.push({file: dir === "." ? e.name : `${dir}/${e.name}`, bytes: e.bytes});
return {steps: node.steps.length, files: sizes.length, bytes: sizes.reduce((sum, f) => sum + f.bytes, 0), largest: sizes.sort((a, b) => b.bytes - a.bytes).slice(0, 10)};

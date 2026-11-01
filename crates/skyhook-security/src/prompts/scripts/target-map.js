const map = [/* {title, body, patterns: ["dir/**", ...]} from the inventory */];
const owned = new Set(), recorded = [];
for (const c of map) {
  const mine = [];
  for (const pattern of c.patterns) for (const f of (await tool.glob({pattern, path: "."})).unwrap().paths) if (!owned.has(f)) { owned.add(f); mine.push(f); }
  if (mine.length) recorded.push((await tool.research({action: "upsert", kind: "component", title: c.title, body: c.body, files: mine})).unwrap());
}
const unassigned = (await tool.glob({pattern: "**/*", path: "."})).unwrap().paths.filter(f => !owned.has(f));
return {components: recorded, unassigned: unassigned.length, sample: unassigned.slice(0, 50)};

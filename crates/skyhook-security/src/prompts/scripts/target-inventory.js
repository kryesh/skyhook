const files = (await tool.glob({pattern: "**/*", path: "."})).unwrap().paths;
const top = [...new Set(files.map(f => f.includes("/") ? f.slice(0, f.indexOf("/")) + "/" : f))];
const byExtension = {};
for (const f of files) { const i = f.lastIndexOf("."); const ext = i > f.lastIndexOf("/") ? f.slice(i) : "(none)"; byExtension[ext] = (byExtension[ext] ?? 0) + 1; }
const manifests = (await tool.glob({pattern: /* build, dependency and deployment manifest names for the languages seen */ "", path: "."})).unwrap().paths;
return {top, files: files.length, byExtension, manifests};

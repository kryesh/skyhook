# Target and component map

Define what this run will analyze and leave a coarse map of the code for every later phase to
navigate by. Start from the user's requested component, entry points, and focus. Do not expand a
narrow request to unrelated services or products.

Establish:

- Included components, languages, entry paths, and important excluded areas.
- The assumed deployment: process boundaries, exposed interfaces, authentication, privileges,
  and relevant configuration or feature choices.
- The deployment facts later phases resolve conditions against, recorded with the scope
  (`add_scope` `deployment`): each build flag or feature option, platform (word size, OS,
  allocator), host behaviour (how an embedding host calls the code, what it limits or checks)
  and input bound (sizes, counts, lengths the deployment allows) the scope admits or excludes,
  one statement each. Spell out what a broad request means: "any embedding host" admits each host
  behaviour the code permits, stated one by one, rather than leaving every deployment question
  open. A fact the user has not settled is asked once here, not left to each reviewer.
- Which actors can supply input and which trust boundaries that input crosses. Record each
  actor that can drive input as a principal with its trust level, and which operations are
  intended for which principal.
- The revision and dirty-state context, available dependencies, and any inaccessible or
  generated code that limits inspection.

Start from a mechanical inventory of the repository rather than a tour: top-level directories,
file counts by extension, and the build, dependency and deployment manifests that reveal
languages, frameworks and process boundaries.

```js
const files = (await tool.glob({pattern: "**/*", path: "."})).unwrap().paths;
const top = [...new Set(files.map(f => f.includes("/") ? f.slice(0, f.indexOf("/")) + "/" : f))];
const byExtension = {};
for (const f of files) { const i = f.lastIndexOf("."); const ext = i > f.lastIndexOf("/") ? f.slice(i) : "(none)"; byExtension[ext] = (byExtension[ext] ?? 0) + 1; }
const manifests = (await tool.glob({pattern: /* build, dependency and deployment manifest names for the languages seen */ "", path: "."})).unwrap().paths;
return {top, files: files.length, byExtension, manifests};
```

Every execution environment, deployment or configuration surface and dependency manifest the
inventory shows is in scope by default, including client-side code and background processing.
Excluding one needs the user's confirmation through the parent, recorded with its reason; only a
region with no reachable code is left out silently. Use `research_query` to reuse existing run
context and `research` to persist the scope and principals. A documented default is not proof
of the deployed setting, so a non-default option belongs in the scope's deployment facts as
admitted or excluded. Ask once, at scope time, how
target-specific policies affect what counts as a reportable finding, and record the answer in
the scope.

Then map the in-scope code into components from the layout and manifests you already read: a
module, a subsystem, a parser or a state machine each, with code that shares state or a contract
kept together. A component's description states its responsibility, which components it hands
work to or takes it from, which ones receive external input, and what about its boundary is
uncertain. Assign files mechanically by directory or module patterns:

```js
const map = [/* {name, description, patterns: ["dir/**", ...]} from the inventory */];
const owned = new Set(), recorded = [];
for (const c of map) {
  const mine = [];
  for (const pattern of c.patterns) for (const f of (await tool.glob({pattern, path: "."})).unwrap().paths) if (!owned.has(f)) { owned.add(f); mine.push(f); }
  if (mine.length) recorded.push((await tool.research({action: "add_component", name: c.name, description: c.description, files: mine})).unwrap());
}
const unassigned = (await tool.glob({pattern: "**/*", path: "."})).unwrap().paths.filter(f => !owned.has(f));
return {components: recorded, unassigned: unassigned.length, sample: unassigned.slice(0, 50)};
```

Code left unassigned is either out of scope with a reason or belongs in a component. The map is
navigation, not a statement of coverage: later phases move files between components until a sweep
covers them. Delegate focused inspection of a boundary to a child launched without a mode, which
holds your tools without this workflow. Ask the parent when ambiguity materially changes
reachability, authorization or research cost; otherwise state the narrow working assumption.

Hand off the scope and component IDs, exclusions with reasons, and open questions. This phase
defines the search boundary; it does not certify that every entry point has been found or
authorize live testing.

# Scripting introduction

Every `script` call gets a fresh QuickJS runtime. Run the example below from a Skyhook source checkout (or adapt the paths to your project).
Tool calls are lazy. Each builder
instance memoizes its own execution, so reusing one builder executes it once while constructing an
equivalent new builder creates a new call:

```js
const packageFile = tool.read({ path: "Cargo.toml" });
const matches = tool.search({ pattern: "TODO", path: "src" });

// The same schema also generates an immutable fluent builder.
const readme = tool.read().path("README.md");

// Builders nested in the returned value are resolved concurrently.
return { packageFile, matches, readme };
```

A workflow file runs in the same runtime:

```sh
skyhook --script workflow.js
# Run headlessly and exit when the workflow and shutdown finish:
skyhook batch --script workflow.js
```

There is no Node.js environment or recursive `script` call. Calls use the same tool schemas,
capabilities, approvals, and persistence as model-originated calls. Read the
[JavaScript reference](../reference/javascript.md) for supported globals, serialization,
concurrency, result wrappers, and failure handling.

For delegation and background work, continue with [jobs and agents](jobs-and-agents.md).
The [tool reference](../reference/tools.md), [HTTP reference](../reference/http.md), and
[saved-output reference](../reference/job-output.md) describe the individual contracts.

# Scripting introduction

Every `script` call gets a fresh JavaScript environment. Run the example below from a Skyhook
source checkout (or adapt the paths to your project). Tool calls are lazy: reusing one builder
executes it once, while constructing an equivalent new builder creates a new call:

```js
// Every direct JavaScript call is a JobView; use unwrap for the native payload.
const packageFile = (await tool.read({ path: "Cargo.toml" })).unwrap();
const matches = (await tool.search({ pattern: "TODO", path: "src" })).unwrap();

// Fluent setters accept the same arguments; each setter returns a new builder.
const readme = (await tool.read().path("README.md")).unwrap();

// Builders can be awaited independently (and can still be batched with Promise.all).
return { packageFile, matches, readme };
```

A workflow file runs in the same runtime:

```sh
skyhook --script workflow.js
# Run headlessly and exit when the workflow and shutdown finish:
skyhook batch --script workflow.js
```

There is no Node.js environment or recursive `script` call. Calls use the same tool schemas,
capabilities, approvals, and saved-output behavior as direct tool calls. Read the
[JavaScript reference](../reference/javascript.md) for supported globals, serialization,
concurrency, result wrappers, and failure handling.

For delegation and background work, continue with [jobs and agents](jobs-and-agents.md).
The [tool reference](../reference/tools.md), [HTTP reference](../reference/http.md), and
[saved-output reference](../reference/job-output.md) describe the individual contracts.

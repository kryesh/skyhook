# Built-in tools

`read`, `search`, `glob`, `exec`, `shell`, `fetch`, `write`, `replace`, `patch`, `remove`, `script`, `targets`,
`target_add`, `skills`, `skill`, `jobs`, `job_output`, `wait`, `ask`, `todo`, and `agent`. `jobs()` lists the current agent's
active jobs, excluding the listing call and its containing script. `jobs({all:true})` includes
completed history; listings contain status and references, never saved results.

`job_output` reads saved output and status immediately; it never waits for new output or completion.
Scripts use `tool.job(id).output(...)`, `.send({value})`, and `.cancel()`.

Use `wait({timeout?: seconds})` (or `await tool.wait(...)` in scripts) to yield until an agent
event or a timeout, then inspect the relevant jobs. Omitted or null `timeout` waits indefinitely;
a supplied timeout must be a positive integer number of seconds.
The result is `{reason:"event"}` or `{reason:"timeout"}`. An event does not guarantee a particular
job has completed; inspect its current status. Waiting does not stop background work.
Do independent work first rather than polling output in a tight loop. Output reads reject the old
`wait` argument.

## Creating files with `write`

`write({path, content})` atomically creates or replaces a UTF-8 file. Set the optional
`create_parents: true` to create missing parent directories recursively before writing, for example
`write({path: "reports/run/summary.md", content: "...", create_parents: true})`.
It defaults to `false`, so existing calls still fail when a parent directory is missing.

## Further contracts

- [JavaScript runtime](javascript.md): fluent builders and native/script result shapes.
- [Jobs and agents](../scripting/jobs-and-agents.md): delegation, input, cancellation, and naming.
- [HTTP requests](http.md): requests, downloads, failures, and security boundaries.
- [Saved job output](job-output.md): file/search shapes, capture, previews, and pagination.
- [Runtime state](runtime-state.md): todos and per-request job snapshots.
- [Execution targets](../guide/execution-targets.md): target selection and workspace inheritance.
- [Instructions and skills](../guide/instructions-and-skills.md): discovery, assets, and copying.
- [MCP](../configuration/mcp.md): importing external tools into the same registry.

All tools remain subject to [permissions and capabilities](../guide/permissions.md), including
calls made inside scripts. A workspace does not sandbox commands or file paths.

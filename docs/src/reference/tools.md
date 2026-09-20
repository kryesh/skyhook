# Built-in tools

`read`, `search`, `glob`, `exec`, `shell`, `fetch`, `write`, `replace`, `patch`, `remove`, `script`, `targets`,
`target_add`, `skills`, `skill`, `jobs`, `job_output`, `wait`, `ask`, `todo`, and `agent`. Calls use the
[common JobView envelope](javascript.md#jobview-response-contract); native payloads are in `.result`.
`jobs()` lists the current agent's active jobs, excluding the listing call and its containing script.
`jobs({all:true})` includes completed history, and the listing payload is in the envelope's `.result`.

`job_output` reads saved output and status immediately; it never waits for new output or completion.
`tool.job(id).output(...)` returns the existing queried view, not another wrapper. Scripts use
`tool.job(id).output(...)`, `.send({value})`, and `.cancel()`.

Use `wait({timeout?: seconds})` (or `await tool.wait(...)` in scripts) to yield until an agent
event or a timeout, then inspect the relevant jobs. The returned view's `.result` is
`{reason:"event"}` or `{reason:"timeout"}`. Omitted or null `timeout` waits indefinitely;
a supplied timeout must be a positive integer number of seconds. An event does not guarantee a
particular job has completed; inspect its current status. See
[waiting for background work](../scripting/jobs-and-agents.md#waiting-for-background-work) for
when a wait resolves.

See the [JavaScript response contract](javascript.md#responseunwrap-and-native-results) for
`response.unwrap()`, serialization, and operational-failure behavior. In this reference,
`tool.job(id).output(...)` and other inspection calls return views for reading their
`presentation` fields; use the task-specific contracts below for their payloads.

## Creating files with `write`

`write({path, content})` atomically creates or replaces a UTF-8 file. Set the optional
`create_parents: true` to create missing parent directories recursively before writing, for example
`write({path: "reports/run/summary.md", content: "...", create_parents: true})`.
It defaults to `false`, so a missing parent directory causes the call to fail.

## Further contracts

- [JavaScript runtime](javascript.md): fluent builders and native/script result shapes.
- [Jobs and agents](../scripting/jobs-and-agents.md): delegation, input, cancellation, and naming.
- [HTTP requests](http.md): requests, downloads, failures, and security boundaries.
- [Saved job output](job-output.md): file/search shapes, capture, previews, and pagination.
- [Runtime state](runtime-state.md): todos and per-request job snapshots.
- [Execution targets](../guide/execution-targets.md): target selection and workspace inheritance.
- [Instructions and skills](../guide/instructions-and-skills.md): discovery, assets, and copying.
- [MCP](../configuration/mcp.md): making external tools available to agents and scripts.

All tools remain subject to [permissions and capabilities](../guide/permissions.md), including
calls made inside scripts. A workspace does not sandbox commands or file paths.

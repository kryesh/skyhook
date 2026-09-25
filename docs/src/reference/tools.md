# Built-in tools

`read`, `search`, `glob`, `exec`, `fetch`, `write`, `replace`, `remove`, `script`, `targets`,
`target_add`, `skill`, `jobs`, `wait`, `ask`, `todo`, and `agent`. Calls use the
[common JobView envelope](javascript.md#jobview-response-contract); native payloads are in `.result`.
`skill` is offered only when skills were discovered.

`exec({command})` runs a string with `/bin/sh -lc`, or an array as a program and arguments without
shell parsing: `exec({command: "cargo test 2>&1 | tail"})` or `exec({command: ["cargo", "test"]})`.

`jobs()` lists the current agent's active jobs, excluding the listing call and its containing script.
`jobs({all:true})` includes completed history, and the listing payload is in the envelope's `.result`.
`jobs({job})` reads that job's saved output and status immediately; it never waits for new output or
completion. It returns the existing queried view, not another wrapper, and accepts the
[output selections](job-output.md) `field`, `start`, `limit`, `pattern`, `context`, and `offset`.
`all` cannot be combined with `job`, and selections require `job`. Scripts also control jobs with
`tool.job(id).send({value})` and `.cancel()`.

Use `wait({timeout?: seconds})` (or `await tool.wait(...)` in scripts) to yield until an agent
event or a timeout, then inspect the relevant jobs. The returned view's `.result` is
`{reason:"event"}` or `{reason:"timeout"}`. Omitted or null `timeout` waits indefinitely;
a supplied timeout must be a positive integer number of seconds. An event does not guarantee a
particular job has completed; inspect its current status. See
[waiting for background work](../scripting/jobs-and-agents.md#waiting-for-background-work) for
when a wait resolves.

See the [JavaScript response contract](javascript.md#responseunwrap-and-native-results) for
`response.unwrap()`, serialization, and operational-failure behavior. In this reference,
`tool.jobs({job})` and other inspection calls return views for reading their
`presentation` fields; use the task-specific contracts below for their payloads.

## Reading and replacing text

`read({path})` reads only regular files. A file is text when it is valid UTF-8 without NUL bytes;
any other file is attached if it is a supported image and rejected otherwise.
`replace({path, old, new, count?})` edits files of at most 4 MiB, both before and after the
replacement.

## Creating files with `write`

`write({path, content})` atomically creates or replaces a UTF-8 file. Set the optional
`create_parents: true` to create missing parent directories recursively before writing, for example
`write({path: "reports/run/summary.md", content: "...", create_parents: true})`.
It defaults to `false`, so a missing parent directory causes the call to fail.

`write({path, source: {path, target?}})` copies another file's bytes exactly, including binary
files of any size. Supply exactly one of `content` or `source`. The destination is always in the
caller's target and workspace. The source is read on its own target: the caller's when `target`
is omitted, or the named one, which requires the `targets` capability and follows the same
workspace rules as any target selection. One approval covers reading the source, connecting to
its target, and writing the destination, except that a remote source path outside that target's
authorization root is approved separately when its target requests it. Contents stream in chunks, through the session host when
either end is remote, and the destination is replaced only once the whole copy has arrived.
For example, an agent on a remote target can copy a file from the session host with
`write({path: "tool.bin", source: {path: "/srv/tools/tool.bin", target: "root"}})`.

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

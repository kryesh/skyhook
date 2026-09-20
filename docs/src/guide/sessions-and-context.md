# Sessions and context

## Session lifecycle

Startup and `/new` open an empty draft without creating a session or its files. The session is
created when you send the first message or explicitly run a script; leaving an unused draft
behind does not create an empty saved session. Resuming an existing session still opens it immediately.

Use `--resume SESSION_ID` to reopen a session. The CLI stores session logs and looks for
saved sessions only in `<workspace>/.skyhook/sessions`, where `--workspace` selects the workspace
(default: the current directory). It does not search parent or sibling workspaces. Sessions
written by an incompatible earlier session format cannot be reopened.

The terminal keeps several sessions open at once. `/new` opens a draft beside the current
session, and `/sessions` lists open sessions, marked with their live status, above the
workspace's saved ones: choosing an open session switches to it, choosing a saved one opens it
alongside. Sessions you switch away from keep working, and the notice line reports when one of
them has a pending question or permission request or has failed. `/close` shuts down the
current session only; quitting shuts down all of them. Model selection
and UI state are described in the [terminal guide](terminal-interface.md#model-selection-and-ui-state).

Interrupting a session stops active model requests and cancels foreground tools, scripts, and
questions. Delegated child agents retain their conversations and can continue with `/retry`;
parents waiting on their work keep those waits. Sending new input lets a parent proceed while
its retained child remains available in the background. Background jobs otherwise keep running,
except those owned by a cancelled script. Explicit job cancellation and session shutdown stop
work rather than leaving it running for later recovery.

Child agents must finish or cancel their owned work before completing. The root agent may leave
background services running while the session remains open, but they do not survive shutdown
or a process restart. On resume, unfinished jobs are marked interrupted and job IDs are not
reused. Retained child agents can be resumed; ordinary process and tool jobs cannot. Pending
child replies remain available after resume. See [jobs and agents](../scripting/jobs-and-agents.md)
for delegation, notifications, and cancellation from scripts.

## Conversation compaction

Skyhook uses the current model to summarize older conversation when context grows large. The
continuation is intended to preserve the task, instructions, plans, findings, unfinished work,
and where to resume. Recent conversation and the original calls for active work remain in
context; historical tool calls are not executed again. Compaction also reconciles the agent's
todo list against the conversation without changing child-agent lists.

Automatic compaction runs after a successful model response whose reported **input + cached
input + output tokens reach 80% of `max_context`**. This is the completed response's usage,
not cumulative session usage or an estimate of the next request. It is independent of
`max_output`. Without reported usage, there is no estimate-based automatic trigger. Tool calls
from the response finish before compaction so their results are available to the summary and
the next request. Text-only responses can also trigger compaction before the turn returns.

The provider still decides whether a request fits. A recognized context-overflow error can
trigger compaction and retry, including for an oversized initial request or new tool output.
Skyhook does not silently reduce `max_output`. The context meter in the terminal is an estimate
for display, not the automatic compaction trigger. Configure
[model limits](../configuration/providers-and-models.md) to match the endpoint you use.

Compaction does not execute tools. It can retain selected job results as well as recent
conversation; full saved results remain accessible through
[`job_output`](../reference/job-output.md). Older details depend on the summary: original
messages remain in the session log, but there is no agent-callable history tool. A validated
summary can still be incomplete or inaccurate.

Invalid, truncated, cancelled, or unsaved summaries leave the preceding context and todos
active. A summary that would not reduce context is skipped rather than installed. The terminal
reports compaction start, estimated input reduction, skips, and failures without printing the
summary. Eligible compaction failures have a bounded retry policy, separate from transient
model failures, which retry until success or cancellation. See
[model failure recovery](../configuration/providers-and-models.md#model-failure-recovery).

For persistence and request reconstruction internals, see
[library architecture](../development/architecture.md) and
[embedding](../development/embedding.md#reconstructing-model-calls).

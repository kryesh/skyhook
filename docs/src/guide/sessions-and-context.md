# Sessions and context

## Session lifecycle

Startup and `/new` open an empty draft without creating a session or its files. The session is
created when you send the first message or explicitly run a script; leaving an unused draft
behind does not create an empty saved session. Resuming an existing session still opens it immediately.

Use `--resume SESSION_ID` to reopen a session. The CLI stores session logs and looks for
saved sessions only in `<workspace>/.skyhook/sessions`, where `--workspace` selects the workspace
(default: the current directory). It does not search parent or sibling workspaces. Sessions
written by an incompatible earlier session format cannot be reopened.

The terminal keeps several sessions open at once. `/new` opens a draft beside the current session,
and `/sessions` lists the open and saved sessions: the current session first, then the other
activated sessions, those still working first, then every other session by most recent activity.
An open session is activated once it has run here: a new session from its first message, a reopened
one from the first action that changes it. Activated sessions are shown in bold. Opening a session
and closing it again with nothing running does not count as activity. The current session is marked `>`. Open sessions show their live status,
and saved ones how they would resume: idle, interrupted, or failed. Opening a saved session changes
nothing in it until you act on it, by sending a message, continuing it, or anything else that is
saved with it; notices about it meanwhile are shown, not saved. A session closed idle with results
still waiting for its main agent is listed and opens as idle, as described below. A lock marks a session
that was held open when the list was shown, by another Skyhook instance or because it was still
closing here. Choosing one that was closing here opens it once it has closed; one another instance
holds cannot be opened until that instance closes it, and choosing it says so. A saved session whose
status cannot be read shows no status symbol. Each row's id, event count, mode, and last activity
time sit at the right. When a row is too narrow, they give way from the left, id first, then event
count, mode, and time, so titles keep at least 30 columns; longer titles are cut short with `…`.
Choosing an open session switches to it, choosing a saved one opens it alongside. Sessions you
switch away from keep working, and the notice line reports when one of them has a pending question
or permission request or has failed. A session you switch away from with nothing running, nothing
waiting on you, and nothing typed or queued is closed, so other Skyhook instances can open it; it
stays in the list as a saved session. `/close` shuts down the current session only; quitting shuts
down all of them.

A session is named after the first message you send it. Rename it with `/rename` (`Ctrl+X L`) or
by clicking its title in the sidebar. A title you set stays until you change it, whatever the
session does later, and renaming starts from it; renaming a session with an automatic title starts
empty, and saving an empty title returns the session to its automatic title. A session
without a title is listed by its first message. Model selection and
UI state are described in the [terminal guide](terminal-interface.md#model-selection-and-ui-state).

Interrupting a session stops active model requests and cancels foreground tools, scripts, and
questions. Nothing asks a model again until you continue with `/retry` or send new input: results
that arrive meanwhile wait, and reach their agent with its next request. A response already
recorded when the interrupt lands stands. Delegated child agents, including ones that had yet to
start, retain their conversations and continue with `/retry`; parents waiting on their work keep
those waits. Sending new input breaks those waits at every depth: the session proceeds, and each
retained child remains interrupted in the background, where `/retry` can still continue it.
Cancelling a job an interrupted agent is waiting on lets that agent go on with the result.
Background jobs otherwise keep running, except those owned by a cancelled script. Explicit job
cancellation stops work for good. Closing a session interrupts it the same way and cancels its
other running jobs: child agents that were running, or already interrupted, can still be continued
after it reopens, as can a delegation it stopped before the child started, which asks for
approval again. Closing a session does not change its last activity time: as after a crash, work
that closing cuts short ends when the session last did something. A session whose journal Skyhook
cannot write stops the same way, as if its process had crashed, and reopens as one would.

Child agents must finish or cancel their owned work before completing. The root agent may leave
background services running while the session remains open, but they do not survive shutdown
or a process restart. On resume, unfinished jobs are marked interrupted and job IDs are not
reused. Requests, jobs, and tool calls the previous process left running are recorded as ending
when it last did something, so their durations stop there and reopening a session does not change
its last activity time. Retained child agents can be resumed; ordinary process and tool jobs
cannot. Pending child replies and job notifications remain available after resume and reach their
agent with its next request: reopening a session does not start one. A session reopened with any
waiting for its main agent resumes interrupted once it runs, so `/retry` delivers them as a new
message would. See
[jobs and agents](../scripting/jobs-and-agents.md) for delegation, notifications, and cancellation
from scripts.

## Conversation compaction

Skyhook uses the current model to summarize older conversation when context grows large. The
continuation is intended to preserve the task, instructions, plans, findings, unfinished work,
and where to resume. Recent conversation and the original calls for active work remain in
context; historical tool calls are not executed again. Compaction also reconciles the agent's
todo list against the conversation without changing child-agent lists.

A successful model response triggers automatic compaction when:

```text
U = input tokens + cached input tokens + output tokens
U >= 0.9 × (max_context - max_output)
```

This is the completed response's reported usage, not cumulative session usage. Skyhook does not
estimate token counts, so without reported usage there is no automatic trigger.
Compaction runs before executing tool calls from that response. The summarizer sees the entire
latest response and its pending calls, explicitly marked as not yet executed. It reconciles the
current todo list with the conversation and any pending todo changes; its reconciled list takes
priority over direct pending `todo` replacements. Those direct replacements fail as superseded,
so they cannot overwrite that list. Todo reads and replacements invoked through scripts run
normally; a scripted replacement can update the reconciled list. The original calls and their
results are included in the first normal post-compaction request; the summary does not consume
those future results. If compaction fails, the turn ends and those calls are recorded as not
executed.
A text-only response returns at once; the compaction it triggers runs before the agent's next
model request, also after the session is closed and reopened. A compaction that is interrupted,
or whose summaries could not be used, stays due and runs again before the agent's next request,
for example when you continue. One that fails for another reason, such as a provider error, is
not tried again, and the next request is sent without it. Switching model or mode clears a due
compaction: from then on, the new model's reported usage decides.

The provider still decides whether a request fits. A recognized context-overflow error can
trigger compaction and retry, including for an oversized initial request or new tool output.
Normal requests keep the configured `max_output`. Compaction requests can use a smaller output
limit when less context space remains, and a summary the provider rejects as too large is retried
with half the output reserve, then with about the older half of the summarized history left out.
After an overflow, compaction keeps only the latest exchange beside the summary. The context meter
in the terminal shows the last reported occupancy. Configure
[model limits](../configuration/providers-and-models.md) to match the endpoint you use.

Compaction does not execute tools. It can retain selected job results as well as recent
conversation; full saved results remain accessible through
[`jobs`](../reference/job-output.md). Older details depend on the summary: original
messages remain in the session log, but there is no agent-callable history tool. A validated
summary can still be incomplete or inaccurate.

Invalid, truncated, cancelled, or unsaved summaries leave the preceding context and todos
active. Every valid, successfully persisted summary installs a checkpoint and its reconciled
todos, even if the replacement context is not smaller. The terminal reports compaction start,
the reported occupancy that triggered it, and failures without printing the summary. Eligible
compaction failures, such as a truncated or invalid summary, have a bounded retry policy,
separate from transient model failures, which retry until success or cancellation. A refused
summary is not retried. See
[model failure recovery](../configuration/providers-and-models.md#model-failure-recovery).

For persistence and request reconstruction internals, see
[library architecture](../development/architecture.md) and
[embedding](../development/embedding.md#reconstructing-model-calls).

# Sessions and context

## Session lifecycle

Startup and `/new` open an empty draft without creating a session or its files. The session is
created when you send the first message or explicitly run a script; leaving an unused draft
behind does not create an empty saved session. Resuming an existing session still opens it immediately.

Use `--resume SESSION_ID` to reopen a session. The CLI stores session logs and looks for
saved sessions only in `<workspace>/.skyhook/sessions`, where `--workspace` selects the workspace
(default: the current directory). It does not search parent or sibling workspaces, and ignores
the library-only `session_root` override.
The terminal keeps several sessions open at once. `/new` opens a draft beside the current
session, and `/sessions` lists open sessions, marked with their live status, above the
workspace's saved ones: choosing an open session switches to it, choosing a saved one opens it
alongside. Sessions you switch away from keep working, and the notice line reports when one of
them has a pending question or permission request or has failed. `/close` shuts down the
current session only; quitting shuts down all of them. Model selection
and UI state are described in the [terminal guide](terminal-interface.md#model-selection-and-ui-state).

Unacknowledged background questions/completions wake their owner for another turn. Child completion
waits for owned work; root background services remain managed by the live session. On resume,
unfinished jobs become interrupted and job identifiers continue monotonically; this does not add
service survival across harness restarts. `SessionHandle::interrupt` stops active provider streams
and cancels the foreground tool, script and question jobs that hold each interrupted turn; the
cancelled calls are recorded as error results. Delegated child agents are left interrupted but
retained, and `/retry` restarts them; an agent waiting on delegated children or background jobs
keeps its wait and, while a child it delegated in the foreground is retained, shows as interrupted.
New input to that agent releases the retained child to the background and proceeds; a resumed
session does the same for a call it settled. Background jobs keep running, except any launched by a cancelled script, which are
cancelled with it. `job_cancel` and shutdown remain the final,
non-resumable paths. Once shutdown is requested no agent begins another model request; a child
reply still pending at that moment stays in the journal and is presented after resume.

## Conversation compaction

After a successful model request completes, Skyhook automatically compacts when the provider's
reported **input + cached input + output tokens reach 80% of `max_context`**. The decision
is independent of `max_output` and does not estimate the next request or newly produced tool
results. Cumulative usage snapshots are not added together; only the completed response's usage
is checked. Without reported usage, there is no estimate-based automatic trigger.

For a tool-calling response, the calls finish and their results are committed to close the
exchange before compaction. The summarizer can see that complete exchange; the next normal
model request receives the retained tool results **after compaction**. Text-only responses
are also checked and compacted before the turn returns or continues with queued input.

The provider still decides whether a request fits. Provider-reported context overflow retains
its separate compaction-and-retry recovery path, including for an oversized initial request or
new tool output. Skyhook does not silently reduce `max_output`. Token estimates remain available
for context display and calibration, but do not control the automatic compaction trigger.

Compaction uses the current model to summarize the conversation. The regular system prompt remains
present, but the summarization request has no tool definitions and explicitly disables tool calls.
Historical tool calls and results remain available as evidence. Signed reasoning that is bound to the
conversation that produced it (Anthropic thinking) is invalidated by compaction, so the summarization
request and retained messages omit it; its visible text and all other reasoning are kept. Subsequent agent requests retain
their normal tool definitions. The directive and resulting
compaction message occupy the user role with separate harness provenance. The model returns a
structured JSON final answer with an objective and resumption point as strings, all other narrative
sections as arrays of strings, a `jobs` array of positive integer job IDs, and a complete current todo list. Empty arrays represent inapplicable
sections. Reasoning is streamed separately and is not parsed as JSON. Skyhook renders entries in
order, separated by blank lines, without rewriting their contents. Entries can include Markdown;
each verbatim plan remains one complete entry, with its status recorded separately.
The response schema is also included in the directive so its descriptions are visible to models
whose provider only uses the schema to constrain decoding. Tool calls returned by the summarizer
are rejected without execution.

Schema property order is preserved through serialization and session replay. The generation order
records the objective, instructions, and plan first, then findings, open issues, running and completed
work, decisions, and recovery context. Todo reconciliation and todos follow that evidence; the
resumption point and next actions come last. Providers such as llama.cpp can enforce this order in
their constrained decoder; JSON Schema itself does not require object property order. The rendered
continuation retains its reading order, with the objective and resumption point near the beginning.

The directive guides the model to carry forward the current task, latest user instructions,
applicable plans verbatim, progress, chosen and rejected approaches with their reasons, and useful
evidence. It includes any preceding continuation in the conversation and asks the model to preserve
still-relevant details, session rules, and the precise resumption point. Completion claims must
reflect observed results and their scope, preserving unfinished investigation, verification, and
uncertainty. The continuation must not invent directions to stop gathering evidence or replace
outstanding work with presentation alone. Schema validation checks
the required fields, their types, todo statuses, and nonblank todo text; it cannot guarantee factual
accuracy or completeness. The prompt prescribes no token budget.

The recent conversation and complete creator exchanges for active jobs remain in context,
including original calls for nested work. Historical calls are not executed again. The summarizer
reconciles this agent's todos against the conversation, accounting for work performed without a todo
update. It preserves unaffected items, retains completed items, and explains changes and their
evidence. The checkpoint installs that list in the todo store without changing child-agent lists.
If todos change during summarization, a fresh attempt uses current history and state. Each working
request still ends with a fresh state block containing current todos and active jobs.

`ModelRequest.response_schema` optionally supplies a named JSON Schema for final answer text,
independently of reasoning settings. Native backends transmit it through OpenAI Chat Completions,
Responses, and Anthropic Messages formats. Codex uses the same Responses codec over
HTTP/SSE, including structured output. Models/endpoints must support the requested features; providers
must reject unsupported constraints instead of silently ignoring them or substituting a prompt.
Ordinary agent requests have no response schema.

Compaction includes a required `jobs` array of job IDs selected by the compactor, or `[]`.
Skyhook supplies those jobs' original parameters and normally truncated outputs in the continuation.
IDs are deduplicated against retained tool results, job notifications, and embedded child results.
These are saved execution facts, not requests to run the jobs again. The snapshots are persisted
with the checkpoint; runtime state and `job_output` provide current status and full results.
Inspection during compaction does not consume pending notifications. Older conversation details
must be preserved in the continuation; original messages remain journaled for host replay, but
there is no callable `history` tool.

```javascript
return tool.job(17).output({field: "/result/stdout", start: 101, limit: 100});
```

Invalid structured responses, truncation, failed persistence, and cancellation leave the preceding
context and todos active. If the continuation and retained messages do not reduce context, Skyhook skips installing it and continues with
the original history. Oversized estimates never cause preserved state to be dropped.

Summarization allows up to three attempts for eligible compaction failures, including invalid or
truncated summaries and summary tool-call responses. Cancellation and non-retryable failures
are not automatically retried. Ordinary model requests instead have separate connection and
context-overflow recovery budgets, allowing up to five combined attempts when both recovery
paths are needed; arbitrary streaming or protocol failures are not automatically replayed.
Recognized provider context-overflow errors force compaction before the next ordinary attempt.
See [model failure recovery](../configuration/providers-and-models.md#model-failure-recovery)
for the retry conditions and budgets.

Each attempt is journaled with its exact input, and failed streamed tool calls are never
executed. The CLI reports compaction start, estimated input reduction, skipped compactions,
failures, and ordinary request retries without printing the summary.

For replay and host-side request reconstruction, see
[embedding](../development/embedding.md#reconstructing-model-calls). The
[runtime-state reference](../reference/runtime-state.md) documents the fresh state included
with each request; it is not another durable message in the journal.

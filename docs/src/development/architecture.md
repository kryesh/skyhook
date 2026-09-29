# Library architecture

The `skyhook-agent` package exposes the `skyhook` library crate. The CLI is a host of that
library, not a separate agent implementation. Provider protocols, terminal interaction, and
local or remote execution meet at shared runtime boundaries.

See [embedding](embedding.md) for host API and replay contracts,
[remote transport and shims](remote-transport-and-shims.md) for execution transport, and
[jobs and agents](../scripting/jobs-and-agents.md) for delegation behavior.

## Ownership boundaries

| Owner | Responsibility |
| --- | --- |
| `config::RuntimeConfig` | An admitted, immutable configuration generation. A selected model retains the configuration that admitted it. |
| `agent::Harness` | Configured models with their provider factories; policy and interaction handlers, configured tools, instructions, and the capability ceiling for new sessions. |
| `agent::SessionHandle` / session runtime | One agent tree, its registry and jobs, target routing, MCP connections, observation stream, and session store. |
| `AgentContext` | One agent loop's projected history, request template, model profile, token calibration, and owned provider context. |
| `provider::ProviderContext` | Conversation-scoped provider resources and one-attempt model invocations, not authoritative conversation history or retry policy. |
| `tool::executor::ToolExecutor` | Shared admission, routing, authorization, and supervised execution for model and script tool calls. |
| `session::SessionStore` | Durable events, content-addressed blobs, and saved job outputs and captures. |

A session's isolated runtime state does not imply filesystem isolation. Execution locations
select machines and workspaces; tools still operate on those machines' files.

## Configuration and provider entries

`config` loads and layers configuration, admits it, resolves model selections, and builds the
model catalog; it does not interpret provider entries. `provider::dialect` owns them
(`RawProviderConfig`, `ProviderSettings`, `ProviderModels`): each dialect module declares its
provider-only `Options` (connection rules, authentication, cross-entry constraints) and its
inheritable request settings. Request defaults and model settings are typed partial declarations
(`provider::settings`) that keep inheritance intent until admission resolves each model into a
complete, independently validated codec and transport profile.

## Provider boundary and lifetime

`provider::Provider` is a shared factory. `open_context(id)` creates an independently owned
`ProviderContext` for one conversation without making a model request; `invoke(&mut self,
request)` returns an owned asynchronous response stream that does not borrow the context. A
startup failure is the stream's first and only item. Providers may send the context identity as
cache-affinity metadata. Custom providers implement both traits. The three inference API
families live under `provider::codec`: each codec encodes requests and decodes native streams
under a typed dialect that names the conventions varying between servers of that family (field
placements, tool-name rules, schema constraint, replay spelling, terminal-event shape), while the
codec keeps the correctness rules and reads error bodies with its API's own codes. Vendor modules
under `provider::dialect` own provider-only settings, the codecs they speak and the dialect
presets for each, credentials, and the transport conventions (fixed headers, per-turn session
headers, and rules and typed readers that map their servers' own error codes and wording onto
error kinds); configured models run through `provider::http::HttpProvider`. Shared request,
message, and response types live under `provider::protocol`.

Each model runs as its own `HttpProvider` at its codec's endpoint under the shared API root; the
models of one provider build share its HTTP client, credential sources, command generations, and
OAuth state.

The runtime retains the provider context across turns, tool work, questions, retries, and
compaction. A completed or failed child invocation can retain its idle agent loop and context
for later input; resources are released when that loop exits. A model change prepares
its replacement before committing the selection, preserves journal history, and resets token
calibration. Reselecting the current model keeps the current context. A mode change also replaces
the prompt and tool surface and invalidates earlier bound reasoning. Resume opens a fresh provider
context under the same context identity.

The session owns the durable conversation (`session::Message`): user text and attachments, the
agent's state (date, live jobs, todos), job events (child replies and presented job views),
parent input and compaction summaries. Providers receive it rendered: state and job events
become runtime text that joins the final turn, so the provider layer knows nothing about jobs
or todos, and those renderers are part of the session format.

Providers receive complete provider-neutral requests; they do not privately replay failed
submissions. A `response_schema` must be transmitted as a structured-output constraint or
rejected with `InvalidRequest`, not silently ignored or replaced with prompting. Input images
must be encoded or rejected explicitly; generated image outputs are not supported. Reasoning
replay carries provider/model provenance, so incompatible private payloads are omitted from
encoding without deleting the durable transcript.

### History, transient state, and caching

A `ModelRequest` separates committed `history` from request-specific `tail` content.
`messages()` iterates history followed by tail. History is an unchanged prefix of later requests
under the same context settings until compaction replaces it; the tail is rebuilt and never
cacheable. A profile's `state_mode` puts runtime state in the tail (`dynamic`), commits it to
history (`persist`), or omits it (`none`). Persisted state lets later requests extend an unchanged
conversation, which matters for reasoning bound to that conversation.

`history_lifetime` communicates cache reuse without prescribing a provider implementation:

- `extends` is the default: later requests extend this history.
- `detached` marks one-off settings, such as a compaction summary request, with no reusable
  history prefix under those settings.

Messages, and Chat dialects that select content-part breakpoints, mark the last history block as a
cache breakpoint except for detached requests.
OpenAI protocols use automatic prefix caching and send the tail last. Their codecs attach a runtime-only tail
to the final tool output or user message instead of introducing a separate user turn, which
would look like the user speaking again after every tool call.

## Request, response, and recovery lifecycle

1. At a request boundary, the runtime applies accepted input and model/mode selections, refreshes
   history from the journal, and builds the request. Shared settings are recorded in
   `model_context`; `model_requested` freezes the exact history range and inline tail.
2. Each provider invocation gets a `model_attempt_started` record for that logical request.
   The stream carries display-only `Delta`s for blocks, cumulative `Usage` snapshots, and one
   terminal `End` holding the `Completion`: the complete items in position order and how the
   response ended (an answer, tool use, or a cut). Only a completion built for tool use carries
   tool calls, so nothing else can authorize execution. `LiveResponse` gives the runtime and
   observers the same provisional view of the deltas until the end arrives. Observers then see
   each response settle as committed, committed-but-aborted, or failed, keyed by the journal
   sequence it produced.
3. Only an accepted response becomes model-visible history. Its message, usage, and outcome
   commit together before tool dispatch. Each tool result commits as its call finishes; request
   projection merges results back into the original call order. This preserves completed work
   after a crash without making completion order part of the next model request.
4. Once a tool exchange is closed, the runtime can compact history or build the next request.
   An agent with no further work returns to its idle loop rather than discarding its context.

`session::RequestLedger` folds these records into one `RequestPhase` per request, the only fold of
the request lifecycle. Observation snapshots embed it, and session statistics, resume settlement
(open attempts to interrupt) and the terminal interface read request outcomes, retry state, usage
and message attribution from it. Folding a record reports the requests it changed, and the ledger
lists the requests changed after a given record, so a host folding records incrementally refreshes
just those. `session::Turns` is likewise the one journal fold of turns, shared by statistics and the
terminal's reply footers: a turn runs from an agent's first agent-purpose (non-compaction) model
request after the last one ended until a response ends it without calling tools or the agent stops.
Whether resuming continues a stopped root turn is a store query that does not decode the session,
shared by resume (`SessionStore::stopped_turn`) and the session summary the terminal's session list
reads. The summary also holds the one rule choosing a session's title from its title records,
reported with the winning title's source, which the terminal reads rather than folding them. Its
last activity skips entry kinds that `EntryKind::is_activity` excludes, through a flag the
`entry_kind` dictionary is seeded with, so the SQL and a host's live fold of
`SessionEvent::is_activity` share one definition. Resume journals `SessionReopened`, then settles
leftover work (interrupted jobs as it restores them, then open requests, calls, and turns) in
appends dated at the journal's newest activity record (`Dated::LastActivity`), so every fold ends
that work there and reopening changes no session's last activity; entry times need not increase with
sequence. Shutdown first closes resumption: a `continue` admitted before it has restarted its jobs,
which count as running, and one after it is refused before journaling a restart. Shutdown then
publishes interruptions it finds still unwinding, held agents' included, cancels only jobs still
running (`CancelScope::Running`), and releases turns held on interrupted ones; a stopping agent
leaves its own interruption suspended. What released turns commit is dated the same way, so closing neither
ends an interruption's resumability nor journals activity for it; explicit cancellation
(`CancelScope::Outcome`) does both. Interrupt and continue read each agent loop's own turn state
instead: idle, busy, parked after a failed or interrupted turn, when only new input resumes it and
job notifications wait for that input's request, or held: interrupted while waiting on retained
children, which `continue` restarts together with it. New input instead breaks every held link
beneath the root: each held descendant's turn ends interrupted, deepest first, so its holder's wait
releases it as a retained child in turn. The host's observation is a display, never an input to
that decision.

Transient recovery belongs to the runtime, not the provider or transport. It classifies
normalized `ProviderErrorKind` values rather than error-message text: rate limits, timeouts,
transport failures, and unavailable servers repeat the frozen logical request. Input
and selection changes wait for the next request boundary. The failed stream is dropped before
backoff; server retry hints take precedence over capped exponential delays. Recovery remains
cancellable and continues until success or interruption.

A retry starts a fresh live response while retaining the attempt audit records and any reported
usage. A stream that fails or closes before its end never authorizes tool execution, and
transient recovery does not rerun completed tools. An expired command-sourced credential is
transient: the provider discards it on a 401 and the retry fetches a fresh one. Other
authentication failures, invalid requests, and protocol errors do not enter this retry loop.
Context-window recovery and unusable compaction summaries have their own bounded recovery path;
a checkpoint that breaks a journal invariant is not retried.

A refusal cut fails without committing the refused message; an abort cut can retain completed
safe content but still fails the turn. Any other response with neither nonblank text nor a tool
call fails with nothing committed: `OutputLimit` when cut at the output limit, `Empty` otherwise.
A truncated or incomplete cut with content commits it as a completed response with that outcome.
Empty text is never committed, even beside a tool call.

## Compaction and context accounting

The journal is authoritative; compaction replaces the model-visible projection, not old records.
Automatic compaction is decided from a successful response's reported occupancy (uncached input,
cached input, and output), at 90% of `max_context - max_output`, not from a display estimate.
It runs after committing that response and before creating or executing its tool calls.

Summarization is a separate recorded request with a structured response schema and no callable
tools. Native history ends before any trailing unanswered tool-call message, keeping the summary
request valid for providers that require paired calls and results. The latest response is instead
supplied in a temporary, journaled tail block: visible text and reasoning plus every pending call's
ID, name, and arguments, marked as unexecuted. Opaque reasoning replay payloads are omitted.
The checkpoint retains the entire available exchange, and every call runs afterward. The
summarizer's reconciled list is authoritative: the todo store records each checkpoint's frontier
and refuses, as superseded, a replacement called directly by a response at or before it.
Replacements made by that response's scripts run normally.
All results join the retained calls in the first normal post-compaction request. Completed
exchanges cannot be partially retained. The retained projection removes reasoning bound to the
replaced context.
Compaction reserves the greater of reported occupancy and calibrated summary-input size,
including the temporary response block, directive, and schema. Its output limit is the smaller
of the configured limit and the remaining context space; no available output space fails before
invoking the provider. Normal request settings are unchanged, and the summary's effective limit
is journaled for exact reconstruction.

History replacement and todo reconciliation become active only after checkpoint persistence.
Validation prevents a stale summary from overwriting newer todo state. Every valid, successfully
persisted continuation replaces the context and reconciles todos, regardless of estimated size
reduction. Replay uses the stored continuation rather than rerunning the summary or today's
renderer. See [model-call reconstruction](embedding.md#reconstructing-model-calls) for the
durable reference contract.

## Tools, jobs, and execution

`ToolRegistryBuilder` supports typed and JSON-based registrations. Typed registrations generate
input and output schemas, and a unit registration completes its job without a result;
registry metadata also supplies compact result documentation to the model
and JavaScript runtime. Both call paths use the same executor, capability checks, authorization
coordinator, and job supervision rather than separate tool implementations. Every call is admitted
once, before authorization: a typed registration may declare an admission step that turns its
parsed input into the value its handler receives, so its checks run at the boundary rather than in
the handler. The admitted value names its path arguments, which are resolved in place, and derives
its permissions, so an inadmissible call never asks for approval. Path admission (`tool::path`)
resolves a requested path where the call runs: relative to the workspace, through existing
symbolic links, as the tool will use it (an existing entry, a write target, or a removal). The
resolved path, not the spelling the caller gave, is what the call asks permission for, over the
exact file or a directory's descendants. The host planner and a remote worker assemble an
invocation's permissions by one rule, and a capability the caller lacks makes the tool unavailable
rather than denied.

The executor coordinates host-owned jobs and persistence. A job moves through one lifecycle
(queued, awaiting approval, running, waiting for input, finished) whose transitions and outcomes
are journaled; the job state a caller sees is a projection of that lifecycle, and a lease typed by
its startup stage carries a job from creation to its running worker. A tool may declare a source argument, a
`TargetPath` on any target: the executor selects its location like a tool target, authorizes the
read with the call, and opens it for the call's context. A host tool whose input places its job
elsewhere, such as a child agent's target and workspace, has that location selected before the job
exists, so every view of the job reports it. Remote contents stream in chunks through the
host, spooled to anonymous files: a remote file through its worker's source request, which the
worker authorizes and reports as the consuming tool, and to a remote handler before its call starts. The local invocation layer
admits typed arguments and runs operations without requiring a session database. The SSH shim reuses that layer
for remote built-ins while the host retains job ownership, policy decisions, and saved output.
This separation keeps remote workers from becoming second agent runtimes.

The host tracks remote readers and accepted-payload ingestion independently of callers and
connection-pool ownership. Pool eviction does not abort this work; after routing stops, transport
resources are released before the finite accepted-payload queue is drained. Session shutdown
fences new connection startup and waits for tracked startup and reader/drain work before backend
cleanup. See [connection and request lifecycle](remote-transport-and-shims.md#connection-and-request-lifecycle).

Tool failures are structured diagnostics: a typed cause plus the operation, subject, site, and
known effects. Tools annotate facts; `tool::diagnostic` owns the one renderer. Facts chosen nearest
the failure win, and each boundary (planning, dispatch, remote result ingestion) only fills what is
still unset: a failure carries a partial context in process and is resolved once, when it is
persisted, sent over the wire, or rendered. A worker's reported site is always rebound to the
destination of the connection it arrived on.

Diagnostics are persisted with the job and rendered per viewer. Target aliases in structured job
diagnostics and result slots registered by their producer, such as an expected `read` failure, are
rendered only when the viewer's capabilities permit exposing them. Host-site wording is relative
to the viewer's target: “on session host” is omitted for host-local viewers and retained for remote
viewers. Pages of a value enclosing a diagnostic slot stream from the viewer's own rendering of
the saved document.

This rendering boundary does not guarantee that target aliases never appear in a restricted view.
A privileged `jobs` output read saves its rendered view as an ordinary JSON snapshot. Later
restricted views of that saved result, or script results that copy it, retain the earlier rendered
JSON unchanged. Already-committed model messages are not rewritten. Arbitrary returned JSON is not
inspected for diagnostics or target aliases, and copied data does not acquire diagnostic provenance.

Saved output and model-visible presentation are separate. A finished result is saved as a compact
document whose large parts live in captures: referenced streams, strings over 4 KiB, and the
outermost larger containers, unless they enclose a capture, JSON-declared text, a complete field,
a long string reached through object members alone, or the diagnostic slot. Text a schema
declares as `contentMediaType: application/json` is classified once at finalization, unless its
output was cut off, and when it is JSON is read as that value from its original bytes.
Finalization also records the fields presentation never shortens, those marked
`x-skyhook-complete` in the output schema joining those a script's returned tool results carry,
and then the size of the result's automatic presentation, which notification batching budgets
without previewing it again. Jobs persist their output schemas so resumed and remote results use
the same rules.

All saved JSON is read through streaming readers built in `job::output::json`. Previews, pages,
and navigation into stored containers hold a bounded pool of what they may show rather than the
value, and sampled readings take stored text only up to its prefix, with its extent from storage.
Memory still grows with the largest single key or number, which the reader buffers whole; with the
keys of each open object while detection tracks duplicates by hash; with fields marked complete,
which are read whole; and with a JSONPath query's selected field, which is loaded within a cap
metered on the bytes it reads. JavaScript receives complete results, while the journal retains the
exact previews delivered to the model. See [saved job output](../reference/job-output.md) for the
user-facing limits.

Child messages and job notifications use a prepared delivery receipt: snapshot pending events,
then commit them to the parent's history together with their acknowledgment. Abandoned preparation leaves the
events pending; cancellation cannot split an accepted commit from its acknowledgment. Replay
recovers pending child messages from durable history, independently of output inspection.

Process tools never connect stdin, and capture stdout/stderr. Without the `Interactive` capability,
they also detach from the controlling terminal on Unix and replace inherited askpass settings with
a rejecting helper. Redirecting stdio alone is insufficient: a program could still prompt through
`/dev/tty`. The helper remains alive through process cleanup, independently of whether target
management is enabled. OpenSSH runs a Skyhook binary as its askpass helper, so every binary calls
`remote::askpass_main()` first: it answers the prompt and exits when started in that role and
returns otherwise. SSH's host-mediated prompt channel is described under [authentication and prompt
isolation](remote-transport-and-shims.md#authentication-and-prompt-isolation).

## Durability and resume

Each durable session uses one SQLite database, `session.db`. The event ledger is append-only and
normalized; blobs, job results, and line-addressable captures share the database rather than
separate transcript or output files. Only shapes that a model, a tool's own schema or a provider
defines are stored as JSON text: tool-call arguments and results, job arguments and output
schemas, tool and response schemas, reasoning replay payloads, saved job results and the
results and pages a job event presented. Related state transitions are appended transactionally
before their in-memory projections are published. Accepted writer work is owned independently
of the caller, so dropping an await does not cancel a commit already in progress.

A writer lock excludes competing session owners, not read-only snapshot inspection. Closing the
store rolls back any abandoned transaction and makes its shared SQLite connection read-only before
releasing that lock, so retained output handles cannot write into a reopened session. Saved output
can still be read. If a write's outcome becomes uncertain, the writer requires recovery
instead of allowing later appends to pretend the commit failed. On resume, the runtime reconciles unfinished model attempts and
committed tool calls lacking results before any agent runs. Agents retain their journaled prompt,
tools, and capability contract; current configuration can narrow that contract, not silently
expand it. Observation is a host projection of durable records plus live state, not a second
source of conversation history.

Source: [agent runtime](https://github.com/kryesh/skyhook/tree/main/src/agent/runtime),
[providers](https://github.com/kryesh/skyhook/tree/main/src/provider),
[tools](https://github.com/kryesh/skyhook/tree/main/src/tool), and
[sessions](https://github.com/kryesh/skyhook/tree/main/src/session).

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
| `agent::Harness` | Shared provider factories, model profiles, policy and interaction handlers, configured tools, instructions, and the capability ceiling for new sessions. |
| `agent::SessionHandle` / session runtime | One agent tree, its registry and jobs, target routing, MCP connections, observation stream, and session store. |
| `AgentContext` | One agent loop's projected history, request template, model profile, token calibration, and owned provider context. |
| `provider::ProviderContext` | Conversation-scoped provider resources and one-attempt model invocations, not authoritative conversation history or retry policy. |
| `tool::executor::ToolExecutor` | Shared admission, routing, authorization, and supervised execution for model and script tool calls. |
| `session::SessionStore` | Durable events, content-addressed blobs, and saved job outputs and captures. |

A session's isolated runtime state does not imply filesystem isolation. Execution locations
select machines and workspaces; tools still operate on those machines' files.

## Provider boundary and lifetime

`provider::Provider` is a shared factory. `open_context(id)` creates an independently owned
`ProviderContext` for one conversation without making a model request; `invoke(&mut self,
request)` returns an owned asynchronous response stream that does not borrow the context. A
startup failure is the stream's first and only item. Backends may send the context identity as
cache-affinity metadata. Custom providers implement both traits; native adapters and codecs
live under `provider::backends`, while shared request, message, and response types live under
`provider::protocol`.

The runtime retains the provider context across turns, tool work, questions, retries, and
compaction. A completed or failed child invocation can retain its idle agent loop and context
for later input; resources are released when that loop exits. A model-profile change prepares
its replacement before committing the selection, preserves journal history, and resets token
calibration. Equivalent profiles keep the current context. A mode change also replaces the
prompt and tool surface and invalidates earlier bound reasoning. Resume opens a fresh provider
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

`history_lifetime` communicates cache reuse without prescribing a backend implementation:

- `extends` is the default: later requests extend this history.
- `detached` marks one-off settings, such as a compaction summary request, with no reusable
  history prefix under those settings.

Anthropic marks the last history block as a cache breakpoint except for detached requests.
OpenAI protocols use automatic prefix caching and send the tail last. Their codecs attach a runtime-only tail
to the final tool output or user message instead of introducing a separate user turn, which
would look like the user speaking again after every tool call.

## Request, response, and recovery lifecycle

1. At a request boundary, the runtime applies accepted input and model/mode selections, refreshes
   history from the journal, and builds the request. Shared settings are recorded in
   `model_context`; `model_requested` freezes the exact history references and inline tail.
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

Hosts fold these records into one `RequestPhase` per request in `session::RequestLedger`;
session statistics and the terminal interface read request outcomes, retry state, and
message attribution from that fold rather than deriving them from live activity. Folding a
record reports the requests it changed, so a host can refresh just those.

Transient recovery belongs to the runtime, not the provider or transport. It classifies
normalized `ProviderErrorKind` values rather than error-message text: rate limits, timeouts,
transport failures, and unavailable servers repeat the frozen logical request. Input
and selection changes wait for the next request boundary. The failed stream is dropped before
backoff; server retry hints take precedence over capped exponential delays. Recovery remains
cancellable and continues until success or interruption.

A retry starts a fresh live response while retaining the attempt audit records and any reported
usage. A stream that fails or closes before its end never authorizes tool execution, and
transient recovery does not rerun completed tools. Authentication, invalid requests, and protocol
errors do not enter this retry loop. Context-window recovery and invalid compaction summaries
have their own bounded recovery path. A refusal cut fails without committing the refused message;
an abort cut can retain completed safe content but still fails the turn; a truncated or
incomplete cut commits its retained content as a completed response with that outcome.

## Compaction and context accounting

The journal is authoritative; compaction replaces the model-visible projection, not old records.
Automatic compaction is decided from a successful response's reported occupancy (uncached input,
cached input, and output), not from a display estimate or output-token limit.

Summarization is a separate recorded request with a structured response schema and no callable
tools. The runtime retains whole exchanges rather than orphaning tool results, and removes reasoning
bound to the replaced context from the retained projection.

History replacement and todo reconciliation become active only after checkpoint persistence.
Validation prevents a stale summary from overwriting newer todo state. A continuation that does
not shrink the context is skipped; replay uses the stored continuation rather than rerunning the
summary or today's renderer. See [model-call reconstruction](embedding.md#reconstructing-model-calls)
for the durable reference contract.

## Tools, jobs, and execution

`ToolRegistryBuilder` supports typed and JSON-based registrations. Typed registrations generate
input and output schemas; registry metadata also supplies compact result documentation to the
model and JavaScript runtime. Both call paths use the same executor, capability checks,
authorization coordinator, and job supervision rather than separate tool implementations.
A tool's argument checks (validation, path arguments, derived permissions) receive its parsed
input and run before authorization. The host planner and a remote worker assemble an
invocation's permissions by one rule, and a capability the caller lacks makes the tool
unavailable rather than denied.

The executor coordinates host-owned jobs and persistence. A job moves through one lifecycle
(queued, awaiting approval, running, waiting for input, finished) whose transitions and outcomes
are journaled; the job state a caller sees is a projection of that lifecycle, and a lease typed by
its startup stage carries a job from creation to its running worker. A tool may declare a source argument, a
`TargetPath` on any target: the executor selects its location like a tool target, authorizes the
read with the call, and opens it for the call's context. Remote contents stream in chunks through the
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
viewers. Capability-sensitive enclosing output pages use private file-backed renderings rather
than shared render caches, keeping captured output out of memory without reusing privileged
renderings for those diagnostic slots.

This rendering boundary does not guarantee that target aliases never appear in a restricted view.
A privileged `jobs` output read saves its rendered view as an ordinary JSON snapshot. Later
restricted views of that saved result, or script results that copy it, retain the earlier rendered
JSON unchanged. Already-committed model messages are not rewritten. Arbitrary returned JSON is not
inspected for diagnostics or target aliases, and copied data does not acquire diagnostic provenance.

Saved output and model-visible presentation are separate. Automatic previews shorten only fields
marked `x-skyhook-truncatable` in the output schema; storing a field separately does not make it
truncatable. Jobs persist their output schemas so resumed and remote results use the same rules.
JavaScript receives complete results, while the journal retains the exact previews delivered to
the model. See [saved job output](../reference/job-output.md) for the user-facing limits.

Child messages and job notifications use a prepared delivery receipt: snapshot pending events,
then commit them to the parent's history together with their acknowledgment. Abandoned preparation leaves the
events pending; cancellation cannot split an accepted commit from its acknowledgment. Replay
recovers pending child messages from durable history, independently of output inspection.

Process tools never connect stdin, and capture stdout/stderr. Without the `Interactive` capability, they
also detach from the controlling terminal on Unix and replace inherited askpass settings with a
rejecting helper. Redirecting stdio alone is insufficient: a program could still prompt through
`/dev/tty`. The helper remains alive through process cleanup, independently of whether target
management is enabled. SSH's host-mediated prompt channel is described under
[authentication and prompt isolation](remote-transport-and-shims.md#authentication-and-prompt-isolation).

## Durability and resume

Each durable session uses one SQLite database, `session.db`. The event ledger is append-only and
normalized; blobs, job results, and line-addressable captures share the database rather than
separate transcript or output files. Only shapes that a model, a tool's own schema or a provider
defines are stored as JSON text: tool-call arguments and results, job arguments and output
schemas, tool and response schemas, reasoning replay payloads, saved job results and the
results and pages a job event presented. Related state transitions are appended transactionally
before their in-memory projections are published. Accepted writer work is owned independently
of the caller, so dropping an await does not cancel a commit already in progress.

A writer lock excludes competing session owners, not read-only snapshot inspection. If a write's
outcome becomes uncertain, the writer requires recovery instead of allowing later appends to
pretend the commit failed. On resume, the runtime reconciles unfinished model attempts and
committed tool calls lacking results before any agent runs. Agents retain their journaled prompt,
tools, and capability contract; current configuration can narrow that contract, not silently
expand it. Observation is a host projection of durable records plus live state, not a second
source of conversation history.

Source: [agent runtime](https://github.com/kryesh/skyhook/tree/main/src/agent/runtime),
[providers](https://github.com/kryesh/skyhook/tree/main/src/provider),
[tools](https://github.com/kryesh/skyhook/tree/main/src/tool), and
[sessions](https://github.com/kryesh/skyhook/tree/main/src/session).

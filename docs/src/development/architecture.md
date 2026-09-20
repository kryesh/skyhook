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

`provider::Provider` is a shared factory. `open_context(correlation)` creates an independently
owned `ProviderContext` without making a model request; `invoke(&mut self, request)` returns
owned startup work and then an owned asynchronous response stream. Neither borrows the context.
The request's correlation must match the context's identity. Custom providers implement both
traits; native adapters and codecs live under `provider::backends`, while shared request,
message, and response types live under `provider::protocol`.

The runtime retains the provider context across turns, tool work, questions, retries, and
compaction. A completed or failed child invocation can retain its idle agent loop and context
for later input; resources are released when that loop exits. A model-profile change prepares
its replacement before committing the selection, preserves journal history, and resets token
calibration. Equivalent profiles keep the current context. A mode change also replaces the
prompt and tool surface and invalidates earlier bound reasoning. Resume opens a fresh provider
context under the same agent correlation identity.

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

- `continuing` is the default: later requests extend this history.
- `ending` marks history expected to be replaced after the request; the runtime sets it when
  the request estimate reaches the compaction threshold. It is a cache hint, not the decision
  to compact.
- `detached` marks one-off settings, such as a compaction summary request, with no reusable
  history prefix under those settings.

Anthropic marks the last history block as a cache breakpoint except for detached requests.
Ending history still needs that breakpoint to read an existing cache entry. OpenAI protocols
use automatic prefix caching and send the tail last. Their codecs attach a runtime-only tail
to the final tool output or user message instead of introducing a separate user turn, which
would look like the user speaking again after every tool call.

## Request, response, and recovery lifecycle

1. At a request boundary, the runtime applies accepted input and model/mode selections, refreshes
   history from the journal, and builds the request. Shared settings are recorded in
   `model_context`; `model_requested` freezes the exact history references and inline tail.
2. Each provider invocation gets a `model_attempt_started` record for that logical request.
   `ResponseEvent`s describe ordered items and blocks: start, typed deltas, authoritative final
   block content, item completion or discard, usage snapshots, and a terminal stop reason.
   `ResponseAssembler` validates this lifecycle and supplies the same snapshot semantics to
   the runtime and observers. Usage snapshots replace earlier values; they are not increments.
3. Only an accepted response becomes model-visible history. Its message, usage, and outcome
   commit together before tool dispatch. Each tool result commits as its call finishes; request
   projection merges results back into the original call order. This preserves completed work
   after a crash without making completion order part of the next model request.
4. Once a tool exchange is closed, the runtime can compact history or build the next request.
   An agent with no further work returns to its idle loop rather than discarding its context.

Transient recovery belongs to the runtime, not the provider or transport. It classifies
normalized `ProviderErrorKind` values rather than error-message text: rate limits, timeouts,
transport failures, and retryable response failures repeat the frozen logical request. Input
and selection changes wait for the next request boundary. The failed stream is dropped before
backoff; server retry hints take precedence over capped exponential delays. Recovery remains
cancellable and continues until success or interruption.

A retry starts a fresh live assembler while retaining the attempt audit records and any reported
usage. Partial failed responses never authorize tool execution, and transient recovery does
not rerun completed tools. Authentication, invalid requests, and protocol errors do not enter
this retry loop. Context-window recovery and invalid compaction summaries have their own bounded
recovery path. Refusals fail without committing the refused message; a provider abort can retain
completed safe content but still fails the turn and never executes its tool calls.

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

The executor coordinates host-owned jobs and persistence. The local invocation layer admits typed
arguments and runs operations without requiring a session database. The SSH shim reuses that layer
for remote built-ins while the host retains job ownership, policy decisions, and saved output.
This separation keeps remote workers from becoming second agent runtimes.

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
normalized; blobs, job output documents, and line-addressable captures share the database rather
than separate transcript or output files. Related state transitions are appended transactionally
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

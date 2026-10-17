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
| `agent::Harness` | Configured models with their provider factories; policy and interaction handlers, configured tools, instructions, modes, and the capability ceiling every session it opens runs under. |
| `agent::SessionHandle` / session runtime | One agent tree, its registry and jobs, target routing, MCP connections, observation stream, and session store. |
| `AgentContext` | One agent loop's projected history, request settings (its `ModelContext`), and owned provider context. |
| `provider::ProviderContext` | Conversation-scoped provider resources and one-attempt model invocations, not authoritative conversation history or retry policy. |
| `tool::executor::ToolExecutor` | Shared admission, routing, authorization, and supervised execution for model and script tool calls. |
| `session::SessionStore` | Durable events, content-addressed blobs, and saved job outputs and captures. |

A session's isolated runtime state does not imply filesystem isolation. Execution locations
select machines and workspaces; tools still operate on those machines' files.

## Configuration and provider entries

`config` loads and layers configuration, admits it, resolves model selections, and builds the
model catalog; it does not interpret provider entries. Layering takes each named entry whole from one
file and stamps every provider with that file (`ConfigHome`), beside which its Skyhook-managed
credentials live. `provider::dialect` owns the entries
(`RawProviderConfig`, `ProviderSettings`, `ProviderModels`): each dialect module declares its
provider-only `Options` (connection rules and any Skyhook-managed `Login`) and its
inheritable request settings. Request defaults and model settings are typed partial declarations
(`provider::settings`) that keep inheritance intent until admission resolves each model into a
complete, independently validated codec and transport profile. The shared `Request` also carries
`service_tier`, a body field on every codec, `explicit_caching` for Chat and Responses, and the
Responses-only selections (`reasoning_summary`, `access_programs`); admission refuses an effective
one on any other codec, and each dialect's Responses preset declares its summary default.

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
headers, body fields the vendor takes whatever the codec, and typed readers that map their
servers' own error codes and wording onto error kinds); configured models run through `provider::http::HttpProvider`. Shared request,
message, and response types live under `provider::protocol`.

Each model runs as its own `HttpProvider` at its codec's endpoint under the shared API root; the
models of one provider build share its HTTP client, credential sources, command generations, and
OAuth state.

The runtime retains the provider context across turns, tool work, questions, retries, and
compaction. A completed or failed child invocation can retain its idle agent loop and context
for later input; resources are released when that loop exits. A model change prepares its
replacement before committing the selection and preserves journal history. Reselecting the
current model keeps the current context. A mode change also replaces the prompt and tool surface
and invalidates earlier bound reasoning. Resume opens a fresh provider context under the same
context identity.

The session owns the durable conversation (`session::Message`): user text and attachments, job
events (child replies and presented job views), parent input and compaction summaries, assistant
items, tool results, and the agent's state (date, live jobs, todos) as a message of its own. Providers receive it rendered:
state becomes a context state entry and job events runtime text, so the provider layer knows
nothing about jobs or todos, and those renderers are part of the session format.

Providers receive complete provider-neutral requests; they do not privately replay failed
submissions. A `response_schema` must be transmitted as a structured-output constraint or
rejected with `InvalidRequest`, not silently ignored or replaced with prompting. Input images
must be encoded or rejected explicitly; generated image outputs are not supported. Reasoning
replay carries provider/model provenance: replay from another model or API is omitted from
encoding, and so is sealed (signed or encrypted) replay from another provider configuration,
without deleting the durable transcript.

### History, transient state, and caching

A `ModelRequest` carries its context as one ordered list of typed entries: messages, and runtime
state the harness wrote, with how long the state stays in the model's view (`StateLifetime`). A
profile's `state_mode` sends each request's own state (`current`, `dynamic`), commits it to the
conversation (`persisted`, `persist`), or omits it (`none`). Everything before the first current
state is a prefix later requests extend under the same context settings until compaction replaces
it. Only a state whose request produced no response later drops out of it, since nothing is bound
to it.

The journal records each request's tail. The projection gives each committed response the state
its request sent, attributed as the request ledger attributes responses, and later requests send
it again before that response as `superseded` state, back to the latest checkpoint or mode switch
(where bound reasoning ends). A superseded state never reaches the model, so the model always
reads only the latest dynamic state. Each codec places state in its dialect's `StateRole`: the
protocol's harness role, or user content where a server has none mid-conversation; never inside
tool output. A Messages dialect may make system-role state turn-scoped, which lets that codec
resend superseded state hidden from the model and keep the conversation unchanged for reasoning
bound to it; every other placement omits superseded state. A state not followed by a response or
the end of the context is omitted everywhere: its request produced nothing bound to it.

`history_lifetime` communicates cache reuse without prescribing a provider implementation:

- `extends` is the default: later requests extend this history.
- `detached` marks one-off settings, such as a compaction summary request, with no reusable
  history prefix under those settings.

Messages, Chat dialects that send `cache_control`, and Chat and Responses under
`explicit_caching` (`prompt_cache_options: {mode: "explicit"}`, marked with
`prompt_cache_breakpoint`) mark cache breakpoints: the cached segments of the system prompt (on
Messages and Chat), the end of the history prefix, and the point before each earlier assistant
response in it, where each prior request's prefix ended, so that prefix stays reusable. Each
dialect caps the breakpoints of one request, system segments included, at four by default, the
documented limit of each codec's API (four cache breakpoints on Messages, four cache writes per
explicit-cache request on Chat and Responses); over the cap, the oldest history breakpoints are
left out, and a request whose system segments leave no room for its history is refused. Responses
sends `cache_control` top-level instead, so the server places the breakpoint itself, and only under
automatic caching; Chat and Responses otherwise use automatic prefix caching. A detached request
marks no history, and the current state and the rest of the request carry no marker. Responses
sends every tool output as a text-block array in explicit mode, so it can carry a marker; Chat
gives that form only to a tool output it marks.

## Request, response, and recovery lifecycle

1. At a request boundary, the runtime applies accepted input and model/mode selections, every
   batch queued by then committed with its selection as one submission, refreshes history from the
   journal, and builds the request. Shared settings are recorded in
   `model_context`; `model_requested` freezes the exact history range and inline tail.
2. Each provider invocation gets a `model_attempt_started` record for that logical request.
   The stream carries display-only `Delta`s for blocks, cumulative `Usage` snapshots, and one
   terminal `End` holding the `Completion`: the complete items in position order and how the
   response ended (an answer, tool use, or a cut). Only a completion built for tool use carries
   tool calls, so nothing else can authorize execution. `LiveResponse` gives the runtime and
   observers the same provisional view of the deltas until the end arrives. Observers then see
   each response settle as committed or failed, keyed by the journal sequence it produced.
3. Only an accepted response becomes model-visible history. Its message, usage, and outcome
   commit together before tool dispatch. Each tool result commits as its call finishes; request
   projection merges results back into the original call order. This preserves completed work
   after a crash without making completion order part of the next model request.
4. Once a tool exchange is closed, the runtime can compact history or build the next request.
   An agent with no further work returns to its idle loop rather than discarding its context.

`session::RequestLedger` folds these records into one `RequestPhase` per request, and
`session::Turns` folds each agent's turns. Statistics, observation snapshots, resume settlement
and the terminal read requests, retries, usage and turns from these two folds rather than
deriving them again.

The core admits every command by the session's `SessionPhase` (viewing, running, shutting down,
failed, closed) in one table in `session/core/phase.rs`, which also decides when what a command
journals is dated. A reopened session is only viewed until the first call that changes it
activates it (`agent/runtime/reopen.rs`), and shutdown stops a session as a crash would
(`agent/runtime/lifecycle.rs`); the host contract for both is under
[interruption, continuation, and shutdown](embedding.md#interruption-continuation-and-shutdown).

Interrupt, continue and new input read three facts the core keeps, and nothing else: the
session's flow (open or paused), each agent's turn (`Turn` in `agent/runtime/agents.rs`), and
whether each child invocation has begun. `stop_turns`, `redirect` and `interrupt_tree` in
`agent/runtime/lifecycle.rs` decide each agent from them. The host's observation is a display,
never an input to that decision.

Transient recovery belongs to the runtime, not the provider or transport. It classifies
normalized `ProviderErrorKind` values rather than error-message text: rate limits, timeouts,
transport failures, and unavailable servers repeat the frozen logical request. Input
and selection changes wait for the next request boundary. The failed stream is dropped before
backoff; server retry hints take precedence over capped exponential delays. Recovery remains
cancellable and continues until success or interruption. A failure is journaled with its retry
and backoff, so a request never reads failed while it retries, and one a crash leaves retrying
reopens interrupted.

A retry starts a fresh live response while retaining the attempt audit records and any reported
usage. A stream that fails or closes before its end never authorizes tool execution, and
transient recovery does not rerun completed tools. An expired command-sourced credential is
transient: the provider discards it on a 401 and the retry fetches a fresh one. Other
authentication failures, invalid requests, and protocol errors do not enter this retry loop.
Context-window recovery and unusable compaction summaries have their own bounded recovery path;
a checkpoint that breaks a journal invariant is not retried. Which cut or empty responses commit
is decided in `agent/runtime/turn.rs`.

## Compaction and context accounting

The journal is authoritative; compaction replaces the model-visible projection, not old records.
Automatic compaction is decided from a completed response's reported occupancy, at 90% of
`max_context - max_output`. Skyhook estimates no token counts: every size it uses is one a
provider reported. Summarization is a separate journaled request with a structured response
schema and no callable tools. Its checkpoint installs the replacement history and the reconciled
todos in one commit, which is refused when it would overwrite newer todo state, and replay uses
the stored continuation rather than rerunning the summary.

`agent/runtime/context.rs` folds the need to compact from the journal. `agent/runtime/compact/`
owns the summary request and how it shrinks after an overflow (`mod.rs`), the retained tail
(`retention.rs`), and the checkpoint (`checkpoint.rs`). The user-visible behavior is under
[conversation compaction](../guide/sessions-and-context.md#conversation-compaction), and the
durable reference contract under [model-call reconstruction](embedding.md#reconstructing-model-calls).

## Tools, jobs, and execution

`ToolRegistryBuilder` registrations are typed; only MCP tools register a server-supplied JSON
schema. Typed registrations generate input and output schemas, and a unit registration completes
its job without a result; registry metadata also supplies compact result documentation to the
model and JavaScript runtime. Both call paths use the same executor, capability checks, authorization
coordinator, and job supervision rather than separate tool implementations. Every call is admitted
once, before authorization: a typed registration may declare an admission step that turns its
parsed input into the value its handler receives, so its checks run at the boundary rather than in
the handler. The admitted value names its path arguments, which are resolved in place, and derives
its permissions, so an inadmissible call never asks for approval. Path admission (`tool::path`)
resolves a requested path where the call runs: relative to the workspace, through existing
symbolic links, as the tool will use it (an existing entry, a write target, or a removal). The
resolved path, not the spelling the caller gave, is what the call asks permission for, over the
exact file or a directory's descendants. A relative path continues the workspace's own resolution,
so every entry looked up on the way counts. A path refused outside the authorization root (one
that does not resolve, or a removal of the root, the workspace or a directory holding either) is
asked for as the path it would be, under a decision that may not ask anyone, and fails only once
allowed, so no prompt or error reveals what lies outside. The host planner and a remote worker assemble an
invocation's permissions by one rule, and a capability the caller lacks makes the tool unavailable
rather than denied.

The executor coordinates host-owned jobs and their persistence. A job's lifecycle (queued,
awaiting approval, running, waiting for input, finished), its startup lease and its single
completion owner live in `job/lifecycle.rs` and `job/supervisor.rs`. A tool may declare a source
argument, a `TargetPath` on any target: the executor selects its location like a tool target,
authorizes the read with the call, and opens it for the call's context. A host tool whose input
places its job elsewhere, such as a child agent's target and workspace, has that location
selected before the job exists, so every view of the job reports it; a local workspace is resolved
and authorized there as one of the call's paths, while a remote one is only resolved where its
worker starts. A job
shows that workspace as written, `..` kept and no link followed, so no view of it reveals or
misstates what lies outside;
the job runs where it resolves. The local invocation layer
(`tool::invocation`) admits typed arguments and runs operations without requiring a session
database. The SSH shim reuses that layer for remote built-ins while the host retains job
ownership, policy decisions, and saved output, so remote workers never become second agent
runtimes. See [remote transport and shims](remote-transport-and-shims.md) for connections,
streaming and shutdown.

Tool failures are structured diagnostics: a typed cause plus the operation, subject, site, known
local effects, and, as a separate fact, whether a sent request may have executed, which only the
sender raises, as it sends. Tools annotate facts; `tool::diagnostic` owns the one renderer. Facts chosen nearest
the failure win, and each boundary (planning, dispatch, remote result ingestion) only fills what is
still unset: a failure carries a partial context in process and is resolved once, when it is
persisted, sent over the wire, or rendered. A worker's reported site is always rebound to the
destination of the connection it arrived on. Diagnostics are persisted with the job and rendered
per viewer; [saved job output](../reference/job-output.md#diagnostics) describes what a viewer
sees.

Saved output and model-visible presentation are separate. A finished result is saved once, as a
compact document with its large parts in captures (`job/output/saved.rs`), and read back through
bounded streaming readers (`job/output/json.rs`). JavaScript receives complete results, while the
journal retains the exact previews delivered to the model.
[Saved job output](../reference/job-output.md) documents the user-facing limits.

Child messages and job notifications commit to the parent's history together with their
acknowledgment, as a model call's result does with the outcome it reports, so an event that never
reaches committed history stays pending for delivery
(`agent/runtime/wait/delivery.rs`, `job/delivery.rs`).

Process tools leave stdin unconnected, capture stdout and stderr, and detach from the controlling
terminal (`tool/builtins/process.rs`), so a program that prompts uses the session's askpass helper,
backed by the host's sensitive prompt handler. Without the `Interactive` capability, a call gets a rejecting
helper instead. OpenSSH and sudo run a Skyhook binary as that helper, so every binary calls
`remote::askpass_main()` first: it answers the prompt and exits when started in that role and
returns otherwise. The helper, its environment and prompt attribution are described under
[authentication and prompt isolation](remote-transport-and-shims.md#authentication-and-prompt-isolation).

## Session core and transaction boundaries

Each durable session is one SQLite database, `session.db`, holding the append-only, normalized
event ledger beside its blobs, saved job results and captures; `session/schema.sql` documents the
tables and the few JSON columns. Each open session has one core thread (`session/core/mod.rs`)
that owns the only writer and the one in-memory fold of the journal: its records, request ledger,
jobs, todos and targets and, never journaled, each running agent's turn and inbox and every
registered wait. Commands run on that thread in order. Each plans against the current state: a
`Plan` names the entries it journals, the output it saves with them, and what it changes live.
The writer journals a plan in one `BEGIN IMMEDIATE … COMMIT` transaction, then applies and
publishes it in one critical section before the next command runs, so no reader sees a record the
state does not reflect, and a rule a concurrent change can break, such as the stale-todos check on
a compaction checkpoint, is decided against the state it commits to. Reopening folds the journal
with the same apply.

One user-visible operation is one plan. A submission is admitted when its messages commit, a
queued batch commits with its model and mode selection in one transaction, and the commit that
ends a turn also closes the calls it left without results and journals the failure that ended
it, right ahead of the turn's end, which is how the request ledger tells a turn that failed on a
request from one that failed before any. The core's turn state is the single
authority for model work: an interrupt changes it in the core, and every commit that starts work
(a request, a model attempt, a response, a delivery, queued input, a compaction, or a job a model
call starts) is refused unless the agent's turn is busy. A turn's cancellation token decides
nothing; it only aborts the I/O in flight, such as a provider stream or a process.

The ledger is only appended to: triggers refuse updates and deletes on every ledger and dictionary
table. A write the journal cannot take stops the session as a crash does. A writer lock excludes
competing session owners, not read-only inspection. Observation is a host view of durable records,
the jobs the session state folds from them, and live state; it is not a second source of
conversation history or job state.

Source: [agent runtime](https://github.com/kryesh/skyhook/tree/main/src/agent/runtime),
[providers](https://github.com/kryesh/skyhook/tree/main/src/provider),
[tools](https://github.com/kryesh/skyhook/tree/main/src/tool), and
[sessions](https://github.com/kryesh/skyhook/tree/main/src/session).

## Terminal transcript selection

The terminal lays out transcript entries into rows. Each row's layout separates what it paints
as decoration (wrap continuations, code padding, disclosure glyphs, spinner gutters, diff markers,
table borders and padding) from the source text it shows. A row's source is described by spans:
which logical line they belong to, which bytes of that line's text, and which bytes of the row
show them. A table cell is a logical line of its own, so the fragments of a wrapped cell share
one line. Spans also keep the source that a row shows differently or not at all, such as a cell's
trimmed separators, a replaced wide grapheme, or an empty cell.

A selection is anchored to source, not to rows: each end is an entry key, a logical line identity
within that entry, and a byte in that line's text. Line identities come from the source and do
not depend on width, but they are identities only. A Markdown line is identified by its source
position. A tool document or plain line after an entry's header is identified by all the text
before it, so inserting a line gives every line below it a new identity; the header's own text
is the entry's state, so it takes no part. Document order is the order in which the
layout first shows each line, which reads a table's cells row by row even when their fragments
interleave on screen. Endpoints, copying and highlighting all read the same spans. Copying joins
each selected line's parts in that order, one line per table cell. Each layout of an entry orders
its lines once, when a selection first needs them, so dragging across a long entry reads only
the rows of the lines it selects.

Whenever entries are laid out again (new text, a reply settling, a resize), the selection is
re-resolved against the new rows. It survives only if its endpoints still exist and the text they
select is unchanged; otherwise it is dropped rather than moved to other text.

Source: [rendering](https://github.com/kryesh/skyhook/tree/main/src/bin/skyhook/tui/render).

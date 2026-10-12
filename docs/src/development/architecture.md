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
| `AgentContext` | One agent loop's projected history, request template, model profile, and owned provider context. |
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
for later input; resources are released when that loop exits. A model change prepares its
replacement before committing the selection and preserves journal history. Reselecting the
current model keeps the current context. A mode change also replaces the prompt and tool surface
and invalidates earlier bound reasoning. Resume opens a fresh provider context under the same
context identity.

The session owns the durable conversation (`session::Message`): user text and attachments, job
events (child replies and presented job views), parent input and compaction summaries, and the
agent's state (date, live jobs, todos) as a message of its own. Providers receive it rendered:
state becomes a context state entry and job events runtime text, so the provider layer knows
nothing about jobs or todos, and those renderers are part of the session format.

Providers receive complete provider-neutral requests; they do not privately replay failed
submissions. A `response_schema` must be transmitted as a structured-output constraint or
rejected with `InvalidRequest`, not silently ignored or replaced with prompting. Input images
must be encoded or rejected explicitly; generated image outputs are not supported. Reasoning
replay carries provider/model provenance, so incompatible private payloads are omitted from
encoding without deleting the durable transcript.

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

Messages, and Chat dialects that select content-part breakpoints, mark the end of the prefix as a
cache breakpoint except for detached requests. OpenAI protocols use automatic prefix caching.

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
   each response settle as committed or failed, keyed by the journal sequence it produced.
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
Whether resuming continues a stopped turn is read from where the journal leaves each agent's last
turn: the core keeps it with each agent's contract, and resume and every agent's observed activity
read it; the session summary the terminal's session list reads applies the same rule to the
database without decoding the session. Resume also parks an idle root interrupted when it has deliveries pending. Whether it has
depends on live-only facts such as resume handlers, so the summary, which reads only the journal,
does not show it, nor does a reopened session until it runs. The summary also holds the one rule choosing a session's title from its title
records, reported with the winning title's source, which the terminal reads rather than folding
them. Its last activity skips entry kinds that `EntryKind::is_activity` excludes, through a flag the
`entry_kind` dictionary is seeded with, so the SQL and a host's live fold of
`SessionEvent::is_activity` share one definition. A reopened session is only viewed: it journals
nothing and runs no agent until the first call that changes it activates it. That call starts the
activation, which runs on even if the call gives up, and a call while it runs joins it. Activation
first prepares the root, so a root that cannot run leaves the session viewed with nothing journaled,
for the next call to try again; then
one commit, which runs to the end once queued, starts the root and settles leftover work: jobs still running as interrupted, open requests, unanswered calls, every turn the
journal leaves open (a request without an outcome, a call without a result, or input not yet
answered), and the releases of the foreground jobs nothing holds across a
restart (retained children, and every outcome nobody read, whoever called the job; a model call cut
short is read by the interrupted answer reopening gives it). Until then a viewed session
reads as that settlement would leave it: stopped at its last activity, its root as it would resume.
The core admits every command by the session's `SessionPhase` (viewing, running, shutting down,
failed, closed), in one match on phase and command that also gives the phase the command moves to;
only a running session that goes on running dates its records now, so what activation settles and
what a closing session journals take its last activity's time: every fold ends that work there,
opening or closing changes no session's last activity, and entry times need not increase with
sequence. Shutdown first moves
the session to shutting down: from then on no agent launches, takes input, begins a turn or requests
a model, and no
`continue` or `send` restarts a job, while work already running may still record how it ends. A
restart committed before that is running work. The same commit stops the session as a crash would:
it interrupts every turn, ends every child invocation still running interrupted, releases the waits
held on them, and cancels only the other running jobs. Closing neither ends an interruption's
resumability nor journals activity for it; explicit cancellation (`CancelScope::Outcome`) does both.
Shutting down a viewed session closes it instead: it never ran, so it stops nothing, journals
nothing, is released for another open, and reads as it did while viewed. A closed session admits
only shutting down and closing, which do nothing.

Interrupt, continue and new input read three facts the core keeps, and nothing else. The session's
flow is open or paused: an interrupt that stops anything pauses it, as does a resume, and until
`continue` or new input opens it no agent takes deliveries, which stay pending for the next request.
Each agent's turn is idle, busy, held, or parked after a failed or interrupted turn, when only input
or `continue` resumes it and job notifications wait for that request. Only a busy turn requests,
commits a response or makes calls. A response without calls leaves busy in its own commit, so an
interrupt either stops a turn or finds it between turns; the turn goes on, busy again, only for
input queued for it or for deliveries while the flow is open, as an agent between turns takes them.
Each child invocation is unbegun until its child takes the input that begins it. An interrupt
decides each agent from its own turn and jobs: a busy turn with a live foreground child invocation
is held on it, any other busy turn is stopped, a stopped turn and an agent between turns give up
input they had yet to take, and an invocation yet to begin is preempted: the input that begins it
runs as an interrupted turn, unless `continue` comes first. A held turn keeps waiting: `continue`,
or cancelling a job it holds, makes it busy again, and a wait that ends otherwise parks it
interrupted. An agent call is accepted by the commit that installs its child, once the call still
runs and its name is free among its owner's installed children. A call without that start record was
never accepted: one a stopped process or new input left interrupted is stopped as a crash stops
it, and is planned and asked for again when the session continues. Only a requested cancellation ends an interruption, and no startup step
restarts a job finished underneath it. New input breaks every held link in one commit: it releases
each held turn's live foreground child invocations, ends the held descendants' turns interrupted,
ends each released invocation the interrupt stopped, held or yet to begin, interrupted, so its call
reads so, opens the flow and queues the input. The host's observation is a display, never an input to that
decision. A release is journaled; from it on, the released job reports to its owner through
delivery, live and after reopening.

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
a checkpoint that breaks a journal invariant is not retried.

A refusal cut fails without committing the refused message. A response the service aborts is a
failed attempt like a server error, retried with nothing committed. Any other response with
neither nonblank text nor a tool call fails with nothing committed: `OutputLimit` when cut at the
output limit, `Empty` otherwise. A truncated or incomplete cut with content commits it as a
completed response with that outcome.
Empty text is never committed, even beside a tool call.

## Compaction and context accounting

The journal is authoritative; compaction replaces the model-visible projection, not old records.
Automatic compaction is decided from a successful response's reported occupancy (uncached input,
cached input, and output), at 90% of `max_context - max_output`. Skyhook estimates no token
counts: every size it uses is one a provider reported.
It runs after committing a response with tool calls, before creating or executing them. A final
answer leaves busy in its own commit, so the compaction it triggers runs at the next request
boundary, before the agent's next turn sends its first request. The need is folded from the
agent's journal whenever its context opens or refreshes: the last committed response's occupancy,
until a checkpoint installs, a round fails with a fault a fresh summary could not fix, or the
agent's model or mode changes. An interrupted round and unusable summaries leave it, so a reopened
or continued agent compacts first, as the live one would have. A summary that overflowed already
shrank within its round, so it consumes the need, even if the round is then interrupted: the next
request fits, or its own overflow compacts. A context-window overflow compacts and retries within its turn only.

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
The summary's output limit is the smaller of the configured limit and the space the completed
response's reported occupancy leaves; no space left fails before invoking the provider. A summary
the provider rejects as too large is retried within the attempt limit, smaller at each step: with
half the output reserve, then also without about half of the history after its opening exchange,
cut at the user or assistant message nearest the middle, which the summary request journals as
its history's first message; no stage raises the output limit an earlier one sent. The retained
tail's sizes are inferred from reports: messages after the agent's last reported request stay
whole, and earlier exchanges join them while the growth between the prompts of the requests
around them fits the retention budget. Only consecutive requests under the same model context
are compared, and growth that is zero or negative sizes nothing, which stops the walk. A
checkpoint for an overflow retains only the last exchange: the messages the overflowing request
added are unsized and may be what overflowed. Retention keeps conversation only; a committed
state is stale once the next request sends its own. Normal request settings are unchanged, and
the summary's effective limit is journaled for exact reconstruction.

History replacement and todo reconciliation become active only after checkpoint persistence.
Validation prevents a stale summary from overwriting newer todo state. Every valid, successfully
persisted continuation replaces the context and reconciles todos. Replay uses the stored
continuation rather than rerunning the summary or today's renderer. See [model-call reconstruction](embedding.md#reconstructing-model-calls) for the
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
its startup stage carries a job from creation to its running worker. A finish claims the job before
it prepares any output, so of competing outcomes (completion, cancellation, interruption) only the one
that publishes saves output, once, in the transaction that journals it; the others wait for it, then
apply only where its outcome admits them,
as a cancellation ends an interruption. Each finish is bound to the invocation it was issued for,
so one from before a restart never ends the invocation the restart began. A tool may declare a source argument, a
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
Finalization also records the fields whose presentation the output schema declares with
`x-skyhook-preview`, joining those a script's returned tool results carry: `complete` fields are
never shortened, and `ends` text keeps its first and last lines; a field declared both ways is
complete. Jobs persist their output schemas so resumed and remote results use the same rules.

All saved JSON is read through streaming readers built in `job::output::json`. Previews, pages,
and navigation into stored containers hold a bounded pool of what they may show rather than the
value, and sampled readings take stored text only up to its prefix, with its extent from storage,
and the last bytes of text declared to keep both ends.
Memory still grows with the largest single key or number, which the reader buffers whole; with the
keys of each open object while detection tracks duplicates by hash; with fields marked complete,
which are read whole; and with a JSONPath query's selected field, which is loaded within a cap
metered on the bytes it reads. JavaScript receives complete results, while the journal retains the
exact previews delivered to the model. See [saved job output](../reference/job-output.md) for the
user-facing limits.

Child messages and job notifications are read, presented, then committed to the parent's history
together with their acknowledgment. The commit is refused when anything it presents is no longer
pending as it was read, as when a claim acknowledged it meanwhile, and the parent presents what is
pending now instead. Abandoned presentation leaves the events pending; cancellation cannot split
an accepted commit from its acknowledgment. A model call's result likewise commits with the
acknowledgment of the outcome it reports, so a result that never commits, as when the process
dies first, leaves that outcome pending for delivery. A job's own reads, such as a `jobs` read or
the calls a script collects, are acknowledged by its finish in the same way, however it ends:
failed or cancelled, with or without output. The host journals no result, so its collection
acknowledges as it reads. Replay recovers pending child messages from durable history,
independently of output inspection. A child's text-only answer is held as its job's result;
when its invocation goes on instead, the answer's release to the owner is journaled too.

Process tools never connect stdin, capture stdout/stderr, and detach from the controlling terminal
on Unix. Redirecting stdio alone is insufficient: a program could still prompt through `/dev/tty`,
and outside the terminal's foreground process group that stops it. Without a terminal, OpenSSH,
sudo and Git prompt through the askpass helper (`SSH_ASKPASS`, `SUDO_ASKPASS`) instead. sudo also
requires some `DISPLAY`, so a placeholder is set only when the process would have none. Root
process tools always receive the session's askpass helper, backed by the host's sensitive prompt
handler. Each prompt names its best-effort requester, and its job when the requesting process descends from
the process a running job registered with the job manager. The session-managed SSH agent replaces
`SSH_AUTH_SOCK` only with the `Targets` capability. Remote workers give their processes a helper that forwards prompts to the host.
Without the `Interactive` capability, a call replaces these settings with a rejecting helper that
remains alive through process cleanup, independently of whether target management is enabled.
OpenSSH and sudo run a Skyhook binary as their askpass helper, so every binary calls
`remote::askpass_main()` first: it answers the prompt and exits when started in that role and
returns otherwise. SSH's host-mediated prompt channel is described under [authentication and prompt
isolation](remote-transport-and-shims.md#authentication-and-prompt-isolation).

## Durability and resume

Each durable session uses one SQLite database, `session.db`. The event ledger is append-only and
normalized; blobs, job results, and line-addressable captures share the database rather than
separate transcript or output files. Only shapes that a model, a tool's own schema or a provider
defines are stored as JSON text: tool-call arguments and results, job arguments and output
schemas, tool and response schemas, reasoning replay payloads, saved job results and the
results and pages a job event presented. Each open session has one core thread that owns the
writer and the one in-memory fold of the journal: its records, request ledger, jobs, todos and
targets, and beside them, never journaled, each running agent's turn and inbox and every
registered wait. A wait, such as an agent's `wait`, a caller awaiting its foreground job, or an
idle agent awaiting work, is a predicate the core decides when it is registered and after every
change. Senders to an agent never wait: its inbox accumulates in order.
Observation views the runtime updates without journaling, such as streaming deltas, sit beside
that fold. A command plans against
that state on the core thread, commits its entries in one transaction, and applies and publishes
them before the next command runs, so no reader sees a record the state does not reflect.
Reopening folds the journal with the same apply, refusing a job transition the job's phase does
not admit. Dropping an await, or its runtime, does not cancel a command already queued.

A write the journal cannot take stops the session as a crash does: a refused or failed
transaction, a commit whose outcome is unknown, a failed blob insert, a finished job's output
that cannot be prepared for its finish, or a panic on the core thread. In the same critical section the session moves to failed, its connection takes no more
writes, everything it runs is cancelled, no agent runs, and every wait and reply resolves with the
failure, which every later write reports too. Observers see the phase change, and read every
turn, request, response and job the session left going as interrupted at its last activity, as
reopening settles them. Rules a concurrent change can break, such as the stale-todos check on a compaction
checkpoint, are decided by their command's plan against the state it commits to, so a refusal
there is not a failure.

A writer lock excludes competing session owners, not read-only snapshot inspection. Closing the
store rolls back any abandoned transaction and makes its shared SQLite connection read-only before
releasing that lock, so retained output handles cannot write into a reopened session. Saved output
can still be read. Dropping the store's last handle closes it the same way, and so does dropping a
session's last handle, which also stops its agent loops, job work, and the approval and question
prompts they asked, where they stand: a session left that way reopens as a crash would leave it. A
session closed with nothing journaled is removed, so a caller that gives up creating one leaves
nothing behind. Closing a failed session releases it too, and reports the failure; reopening
resolves what committed. Activating a reopened session reconciles unfinished model attempts and
committed tool calls lacking results before any agent runs. Agents retain their journaled prompt,
tools, and capability contract; current configuration can narrow that contract, not silently expand
it. Observation is a host view of durable records, the jobs the session state folds from them, and
live state; it is not a second source of conversation history or job state.

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

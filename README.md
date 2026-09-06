# Skyhook

Skyhook is a provider-neutral coding-agent harness with a programmable JavaScript orchestration
runtime. A tool is registered once and is then available through the model tool protocol and as a
lazy builder inside `script`.

The workspace contains the source-only `skyhook-agent-core` library package (whose Rust crate is
named `skyhook`) and the installable `skyhook-agent` CLI package. The core does not depend on a
provider-specific response type; Flux adapters supply OpenAI, Anthropic, Codex/ChatGPT subscription,
Claude subscription, and OpenAI-compatible backends.

## Install

Rust 1.88 or newer is required. Default installs build static Linux shims from source, so install
[`cross`](https://github.com/cross-rs/cross) and start Docker or Podman first:

```sh
cargo install cross --git https://github.com/cross-rs/cross
cargo install skyhook-agent --locked
```

Install directly from Git or a local checkout with:

```sh
cargo install --git https://git.kryesh.tech/Apps/skyhook.git skyhook-agent --locked
cargo install --path . --locked
```

The shims are compiled dynamically for `x86_64-unknown-linux-musl` and
`aarch64-unknown-linux-musl`, validated as static ELF executables, and embedded in the installed
`skyhook` command. No prebuilt shim binaries are stored in Git or the Cargo source package. A
local-only build that does not require Cross is available with `--no-default-features`; SSH use from
that build returns an explicit missing-shim error.

## Run

```sh
cp skyhook.example.toml ~/.config/skyhook/config.toml
export OPENAI_API_KEY=...
skyhook --prompt "inspect this repository"
```

Use `--config path.toml` to use an explicit config instead of the user config, `-m/--model PROFILE`
to override the root model profile, `--approve-all` to skip all tool approval prompts, and `--workspace PATH`
to choose the tool root, and `--resume SESSION_ID` to reopen a durable session. Pass a one-shot
prompt with `-p/--prompt`, or run a JavaScript workflow file through the registered script tool with
`-s/--script`. Without either option, the CLI reads prompts interactively. Its default policy allows
reads, agent state operations, and writes inside the root workspace. Process execution, remote
access, target configuration changes, and writes outside the root workspace require confirmation.
Set top-level `approve_all = true` in the config for the same non-interactive approval behavior.
Streamed assistant messages and concise tool-start summaries are prefixed with their session-local
agent ID (`root`, `1`, `1:1`, and so on), so root and child-agent activity remains distinguishable
during concurrent workflows. When the CLI closes the session, it prints cumulative output, total
input, and uncached input token counts.

## Reconstructing model calls

Session format 1 records the inputs needed to reconstruct each call at the shared `Provider`
boundary. It stores no backend-specific request bodies or authentication headers:

- `model_context` records the configured provider name and a shared `ModelRequest` template:
  actual model ID, assembled system prompt (including harness/profile instructions and location),
  tool descriptions and schemas, optional response schema, reasoning setting, output limit, and correlation. Its `messages`
  array is empty; conversation history remains in `message_committed` and `compaction` events.
- `model_requested` is persisted before each provider invocation. It references the context event's
  sequence and records an ordered list of source-event references and exact inline messages,
  including transient runtime state and compaction directives. Its purpose distinguishes ordinary
  agent calls from summarization. Ordinary calls within a turn share one context record;
  summarization records a separate template containing its response schema.
- `compaction` is an ordinary log event containing the exact replacement message, retained original
  message references, covered frontier, previous compaction reference, and the
  summarization request reference and token estimates. It also contains schema version 1 and the
  reconciled owner todo list. History and todos become active together after persistence.
- Reconstruction resolves recorded references and inline values rather than using current
  configuration, prompt code, or live job state. It neither reruns summarization nor rerenders old
  compaction messages. Failed/interrupted calls retain their input records; usage records identify
  their originating request, including summarization calls.

`session::reconstruct_model_request(&records, sequence)` returns the provider name and reconstructed
`ModelRequest` for a `model_requested` sequence. Image metadata references the existing session blobs;
`store.hydrate_model_request(&mut request).await` restores their payloads when needed. This reconstructs
Skyhook's provider-neutral input, not an API-specific wire encoding. Format versions remain at 1;
there is no compatibility or migration layer for earlier layouts.

## Conversation compaction

Skyhook automatically compacts an agent's conversation when its estimated complete input reaches
the model profile's `max_context - max_output`. This is a soft trigger: estimates never reject
a request locally, and the provider decides whether it fits. Both limits are mandatory; Skyhook
does not silently reduce the output allowance. Token estimates include prompts, tools, messages,
images, response schemas, and current state, and are calibrated against reported provider usage.

Compaction uses the current model to summarize the conversation. The regular system prompt remains
present, but the summarization request has no tool definitions and explicitly disables tool calls.
Historical tool calls and results remain available as evidence. Subsequent agent requests retain
their normal tool definitions. The directive and resulting
compaction message occupy the user role with separate harness provenance. The model returns a
structured JSON final answer with an objective and resumption point as strings, all other narrative
sections as arrays of strings, and a complete current todo list. Empty arrays represent inapplicable
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
independently of reasoning settings. Built-in Flux adapters transmit it through OpenAI Chat
Completions, Responses, and Anthropic Messages formats. Codex OAuth schema calls use HTTP while
ordinary calls retain Flux's WebSocket transport. Models/endpoints must support structured output;
an opaque provider wrapped with `FluxProvider::new` rejects schemas explicitly instead of ignoring
them. Ordinary agent requests have no response schema.

Original messages and saved job artifacts remain available. The `history` tool reads only the
calling agent's conversation, including prior compaction messages and job notifications, without
consuming notifications or exposing reasoning blocks. It supports an exact `source`, a literal
case-insensitive `query`, and up to 100 text chunks per page, with the complete response limited to
8 KiB. Use returned `next_cursor` and `through` together, keeping the same source/query, to browse
a stable snapshot. Returned entry offsets are byte positions in original source text.

```javascript
const page = await tool.history({query: "rejected approach", limit: 10});
return page.next_cursor === null ? page : await tool.history({
  query: "rejected approach", limit: 10, cursor: page.next_cursor, through: page.through
});
```

Sources use `m42/b0` for original text, `m43/b0/result` or `/console` for tool output, and `c50/b0`
for compaction text. Tool-call arguments are available at `m42/b0/arguments`. Existing job retrieval
continues to provide complete saved outputs.

Invalid structured responses, truncation, failed persistence, and cancellation leave the preceding
context and todos active. If the continuation and retained messages do not reduce context, Skyhook skips installing it and continues with
the original history. Oversized estimates never cause preserved state to be dropped. Ordinary model
calls and summarization each allow up to three attempts total, including failures during streaming;
invalid summaries, truncated summaries, or summary tool-call responses are also retried. Recognized provider context-overflow errors force compaction
before the next ordinary attempt. Each attempt is journaled with its exact input, and failed streamed
tool calls are never executed. The CLI reports compaction start, estimated input reduction, skipped
compactions, failures, and ordinary request retries without printing the summary.

## Execution targets

Set top-level `targets_enabled = true` to grant the session's target capability. When it is false
(the default), target-management tools, target arguments, JavaScript target setters, and target
prompt guidance are all omitted from the model-visible surface.

Targets are named directly under `[targets.<name>]`. Importing concrete aliases from the user's
SSH configuration is disabled by default. Session tools can upsert targets without modifying TOML.

```toml
[targets]
import_ssh_config = false

[targets.bastion]
type = "ssh"
host = "bastion.example.com"

[targets.bastion.ssh]
user = "gateway"

[targets.build]
type = "ssh"
host = "build.internal"
workspace = "/srv/project"
via = "bastion"

[targets.build.ssh.auth]
kind = "key"
path = "~/.ssh/build_ed25519"
```

Targets have an explicit `type`. The built-in `root` is `local`, meaning the session host—the machine
running the main Skyhook process. It always identifies that machine, including when an agent's
current target is remote. Named target configuration and `target_add` accept only `type = "ssh"`.
SSH-specific settings live under `ssh`, including authentication kinds `openssh` (default),
`agent` (the session's managed agent), and `key` (an explicit path). Older target configurations and
session formats are rejected; update configurations and start a new session.

The target list shows each target's effective destination hostname/IP, original SSH alias, type,
origin, and `via`. SSH imports resolve aliases with OpenSSH and translate `ProxyJump` into named
`via` chains, creating stable names for unnamed or route-specific hops. Opaque `ProxyCommand`
settings remain SSH-specific. Listing targets does not connect to their destinations.

Agents receive their current target's name, type, resolved hostname, origin, and `via` alongside
their effective workspace in `skyhook_context`. Registering a target does not change the agent's
location.

`via` describes reachability; `origin` identifies the machine where connection configuration and
outbound SSH processes belong. Local config/imports originate on `root`. `target_add` defaults its
origin to the caller's target and accepts an explicit `origin` override:

```javascript
await tool.target_add({name: "database", type: "ssh", host: "db.internal",
  origin: "build", ssh: {auth: {kind: "key", path: "~/.ssh/database"}}});
```

The resulting target automatically uses `via: "build"`, extended by any jump chain in build's SSH
configuration. Registration resolves configuration on the origin, which may require connecting to
it, but never connects to the new destination or decrypts its keys. Registrations are session-wide.
A root-origin connection via build uses native SSH forwarding; a build-origin connection runs SSH
under build's shim. Native forwarding alone does not require a shim on jump hosts. Native hops in
a connection segment must share its configuration origin; use an explicit remote origin to start
a shim-owned continuation. This prevents remote credential paths being interpreted on root.

One private, session-owned SSH agent runs on root. SSH loads keys lazily from their configuration
origin into this central agent. Remote Skyhook commands receive a private relay socket in
`SSH_AUTH_SOCK`, allowing commands such as `git`, `ssh`, and `ssh-add` to use the same identities.
Skyhook does not inherit an existing user agent. Agent state is discarded at session shutdown and
is not restored when resuming a session.

Skyhook's askpass handler routes passwords, key passphrases, keyboard-interactive challenges, host
confirmations, and agent confirmations to the host UI. Interactivity is controlled by the handler,
not target configuration. `-p`, `-s`, and invocations without an interactive terminal fail operations
that require input; `--approve-all` does not approve authentication prompts. Secrets are never
included in tool results or session logs.

Target-aware tools, including `agent`, use these rules:

- Omitting `target` uses the calling agent's target and current workspace.
- Setting `target: "root"` uses the session host and its configured session workspace.
- Setting another named target uses its configured workspace, except that explicitly selecting the
  calling agent's current named target retains that agent's workspace override.

Tools without a target selector use the calling agent's target and workspace. An explicit
`agent.workspace` takes precedence over the selected base; relative overrides resolve against that base.

Prefer these named targets to manually running `ssh`. The first tool in a session that needs an SSH
route asks for approval before any probe, authentication, or shim deployment. Approval is shared
across tools and workspace-specific pooled connections; reconnecting an unchanged route does not
prompt again. Changing a target or any hop in its `via` route invalidates the affected approval.

JavaScript remains host-owned while nested targeted tools inherit their caller's location. The
`agent` tool also accepts a named target or `root`.
Remote agent state machines, providers, approvals, message history, and canonical session logging
remain host-owned. The shim retains only live remote tool/process execution state and unclaimed
background-job results; its worker store is ephemeral.

The CLI injects its generated shim catalog into the core harness. Library embedders receive an empty
catalog by default and can provide their own `EmbeddedShimCatalog` through `HarnessBuilder`; selecting
a platform without a supplied shim returns an unsupported-platform error.

## Configuration

Providers and models are separate named profiles. API secrets are read from environment variables;
they are not stored in the TOML file. `openai_compatible` accepts an optional `api_key_env`, so a
local endpoint can be keyless. Its `api` is either `chat_completions` or `responses`.

```toml
default_model_profile = "local"
approve_all = false
targets_enabled = false

[providers.local]
kind = "openai_compatible"
base_url = "http://127.0.0.1:11434"
api = "chat_completions"

[models.local]
provider = "local"
model = "qwen3-coder"
max_context = 128000
max_output = 16384
supports_images = false
```

Every model profile requires `max_context` and `max_output`. Set them to the context capacity and
output limit you want Skyhook to use for that model; the example values are conservative starting
points. Both must be positive, `max_output` must be smaller than `max_context`, and the output limit
must fit the provider's 32-bit token field. Skyhook sends the configured output limit on every call.

The `codex` and `claude` provider kinds import and refresh credentials through Flux, including
credentials from the official Codex and Claude CLIs. Instructions in `AGENTS.md` files are loaded
from outermost ancestor to workspace, followed by instructions configured through the library.

## Embedded JavaScript

Every `script` call gets a fresh QuickJS runtime. Tool calls are lazy. Each builder
instance memoizes its own execution, so reusing one builder executes it once while constructing an
equivalent new builder creates a new call:

```js
const packageFile = tool.read({ path: "Cargo.toml" });
const matches = tool.search({ pattern: "TODO", path: "src" });

// The same schema also generates an immutable fluent builder.
const firstLines = tool.read().path("README.md").start(1).limit(40);

// Selecting a skill loads its SKILL.md; selecting an asset changes the operation.
const instructions = tool.skill({ name: "release" });
const template = tool.skill({ name: "release", path: "template.md" });

// Builders nested in the returned value are resolved concurrently.
return { packageFile, matches, firstLines, instructions, template };
```

The runtime also exposes:

- `Date`, `RegExp`, `Map`/`Set`, `Proxy`/`Reflect`, and `BigInt`;
- `ArrayBuffer`, `DataView`, and typed arrays, including `Uint8Array.fromBase64`,
  `.fromHex`, `.toBase64()`, and `.toHex()`;
- `performance.now()` for measuring elapsed milliseconds;
- `await sleep(ms)` for asynchronous waits, resolving to `undefined`. The delay must be a finite,
  nonnegative number of milliseconds within the host timer range; fractional values are accepted.
  Sleeps stop when the script is cancelled, and unawaited sleeps do not keep it alive;
- `new WorkPool(concurrency).map(items, worker)` and `.run(tasks)` as async iterables yielding
  successful `{index, value}` results in completion order. Failed items are logged and skipped;
  remaining items continue. Early iterator closure stops scheduling and drains running work;
- `await receive()` for the next JSON input sent to the owning job with
  `tool.job(id).send({value})` (background scripts only).

Read or search saved command output with `tool.job(commandJobId).output({field:"/result/stdout"})`.

Before returning results, convert `BigInt` values to strings, dates with `.toISOString()`, and
typed arrays with `Array.from(bytes)`, `.toBase64()`, or `.toHex()`. The runtime does not provide
Node.js APIs, `fetch`, `URL`, `TextEncoder`/`TextDecoder`, or `setTimeout`/`setInterval`.

`console.log(...values)` captures space-separated text, formatting objects as JSON. The agent
receives it in a separate text block alongside the unchanged JSON return value, on success or
failure. Logs are delivered at completion, not streamed, and are capped at 16 MiB per script with
an explicit truncation marker. Background job envelopes retain them in `console_output` when
nonempty. Scripts without logs keep their existing output format.

Builder setters and object arguments come from the same strict JSON schema; omitted values receive
the handler's normal defaults. Awaiting a builder executes it immediately. Returning builders recursively executes independent
branches concurrently. All executions still pass through the same registry, policy hook, job
supervisor, persistence, and path authorization checks as model-originated calls. Top-level
`undefined` returns JSON `null`; nested `undefined` values are rejected with their result path.
The `script` tool is deliberately omitted from the runtime, preventing recursive script invocation.

Failed tools retain any partial output (including captured process output on timeout). Model tool
errors include it in an `output` field; JavaScript callers can catch the error and read `error.output`.

## Library architecture

- `provider::Provider` returns a boxed asynchronous response stream; concrete
  adapters live under `provider::backends` and wire types under `provider::protocol`.
- `tool::ToolRegistryBuilder` supports typed and JSON-based tools, while `tool::executor::ToolExecutor`
  turns every invocation into a supervised job. Typed registrations generate both input and output
  schemas; compact result shapes are included in model and script documentation.
- `session::SessionStore` persists a versioned append-only JSONL log, content-addressed image blobs, job
  outputs, and cursor-addressable job progress.
- `agent::Harness` owns profiles and policy; each `agent::SessionHandle` owns an isolated agent tree
  and registry.
- Child agents are one-shot, profile-selectable agents. Each model-facing `ask` contains one
  `{id, prompt, options}` question; independent concurrent calls are merged by the runtime. A child
  question batch changes its stable agent job to `waiting_input`; answer that job with
  `tool.job(id).send({value: answer})` in a script. For a merged batch, use
  `tool.job(id).send({value: {question_id: answer, another_id: answer}})`; the keys are the IDs
  included in the waiting job's `questions` output.
  Root-agent questions still go directly to the host question handler.

`agent` accepts an optional `depth` delegation budget. It defaults to zero, making the launched
child a leaf. A caller may grant less than its own available depth; once no depth remains, `agent`
is omitted from both model tools and script bindings. Its optional `workspace` accepts relative or
absolute directories for both local and remote children. Relative overrides resolve against the
workspace selected by the target rules. Children receive a fresh conversation, shared harness
instructions and host-owned skills, and harness model/profile defaults unless overridden. Agents on
the same target share files; a workspace override creates no filesystem isolation. Children must
finish or cancel all owned jobs and descendants before their agent job completes. Only root agents
may leave background services running after answering.

Filesystem tools and command `cwd` accept absolute paths or relative paths including `..`.
The workspace is a base directory, not a security boundary. Canonical paths outside the configured
workspace require approval, cached in memory by session, target, access mode, and path; directory
approval covers descendants. Read and write grants are separate. Process execution remains subject
to confirmation on each invocation.

Background-capable tools accept an optional `bg` argument. Model-facing calls return a job view
with `id`, `tool`, `state`, and `location`, plus a structured `result`. Only annotated fields are shortened; `truncated` lists their continuation cursors.
JavaScript foreground calls return the handler's full native result; background launches return
a job reference. A child question has `state: "waiting_input"`; `job_output` returns its stable
question IDs and text in `question`, regardless of its size.

Jobs normally follow `queued → running → completed`, optionally cycling through
`waiting_input → running`; `failed`, `cancelled`, and `interrupted` are terminal alternatives.
`job_output` acknowledges a pending question or terminal result and suppresses duplicate automatic
notification. Explicit reads remain repeatable. The optional wait duration returns when content,
a question, or completion is available, or when its deadline expires; it does not stop the job.
Unanswered questions do not expire. Send answers to the stable child-agent job ID.

Cancellation cascades through descendant jobs and agents and terminates managed command process
groups locally and remotely. It is a request: use job_output to confirm termination. Deliberately
detached processes and unreachable remote hosts limit cleanup. Command timeouts are optional;
omission or null means no deadline. Explicit timeouts of 1–3600 seconds terminate execution,
retaining captured output. A nonzero command exit is a normal result with `exit_code`.

Approval controls and pending/granted approval details remain user-facing. Agents see an ordinary
queued job while authorization is pending. Denials preserve the reason and add
`code: "permission_denied", executed: false` to the rejected operation's error or job envelope;
script exceptions carry the same fields. A containing script may already have executed other work,
so uncaught script failures retain the rejected operation as nested failure details. Agents must not
circumvent a denial through another tool or route.

Unacknowledged background questions/completions wake their owner for another turn. Child completion
waits for owned work; root background services remain managed by the live session. On resume,
unfinished jobs become interrupted and job identifiers continue monotonically; this does not add
service survival across harness restarts. `SessionHandle::interrupt` stops active provider streams
and cancels jobs across the session's agent tree.

`agent`, `exec`, and `shell` accept an optional `name` describing the work. Names must use
lowercase snake_case: start with a letter, then use lowercase ASCII letters, digits, and single
underscores between nonempty words. Examples include `inspect_config`, `run_tests`, and `build_v2`.
Names are descriptive labels, do not need to be unique, and do not replace job IDs.

```js
return tool.exec({argv: ["cargo", "test"], name: "run_tests", bg: true});
```

Names appear in active-job state, job envelopes (including notifications and inspection), and
durable job records. They survive session resume. Omitted or null names leave jobs unnamed;
invalid names are rejected before execution. JavaScript builders also support `.name("run_tests")`.

## Todos and runtime context

Every agent has an ordered advisory todo list. `todo()` reads the caller's list;
`todo({items:[...]})` replaces the whole list, and an empty array clears it. Each item has
`text` and a `status` of `pending`, `in_progress`, or `completed`. Multiple items may be in
progress. Unfinished items do not prevent an agent from finishing.

Any agent allowed to delegate can seed a child with `agent.todos`. The task `prompt` remains
required; seed strings become pending items before the child's first model request:

```js
const child = await tool.agent({
  prompt: "Implement the requested change and report the validation results.",
  name: "implement_change",
  todos: ["Inspect the implementation", "Make the change", "Run relevant checks"],
  bg: true
});
await tool.job(child.id).output({wait:60});
return tool.todo({job: child.id});
```

The child owns subsequent edits. Ancestors can inspect a descendant using its agent job ID,
including after it finishes; `items` and `job` cannot be combined. Inspection is available once
the child has initialized; a queued launch may not have a list yet. Reads and updates return
`{agent, items}`. An agent without todos has an empty list. Inside the child, progress can be
updated with the same tool:

```js
return tool.todo({items: [
  {text: "Inspect the implementation", status: "completed"},
  {text: "Make the change", status: "in_progress"},
  {text: "Run relevant checks", status: "pending"}
]});
```

Lists persist with the session, including completed and interrupted child lists. Resume preserves
recorded statuses. Host interfaces can read all current lists through `SessionHandle::todos()`
and observe `SessionEvent::TodosReplaced` through the existing runtime event subscription.
The public `TodoItem`, `TodoStatus`, and `TodoSnapshot` types live in `skyhook::agent`.

Each model request ends with one fresh `<skyhook_state>` snapshot containing the host's current
local `date` (`YYYY-MM-DD`), and the caller's `todos` and `active_jobs`, including empty arrays.
The date is refreshed per request instead of being fixed in the system prompt at agent startup.
Active jobs include their execution `location` and `age_seconds`: elapsed whole seconds since
job creation, including time queued or waiting for input. Age is clamped to zero if the clock
moves before the creation timestamp. Child-agent jobs report the child's selected target and
workspace once initialized. Location includes `target` when target capabilities are enabled;
otherwise it contains only `workspace`.

```json
{
  "date": "2026-09-05",
  "active_jobs": [{
    "job": 7,
    "tool": "exec",
    "name": "run_tests",
    "state": "running",
    "location": {"workspace": "/home/user/project"},
    "age_seconds": 12
  }],
  "todos": []
}
```

These snapshots are assembled at request time
and never appended to durable conversation history. Actual job notifications, tool exchanges,
and todo replacement events remain durable. Provider caching behavior is
unchanged; transient history does not guarantee exclusion from provider KV caches.

## Built-in tools

`read`, `search`, `glob`, `exec`, `shell`, `write`, `replace`, `patch`, `remove`, `script`, `targets`,
`target_add`, `jobs`, `job_output`, `ask`, `todo`, and `agent`. `jobs()` lists the current agent's
active jobs, excluding the listing call and its containing script. `jobs({all:true})` includes
completed history; listings contain status and references, never saved results.

`job_output` reads saved output and status, optionally waiting for output, a question, or completion.
Scripts use `tool.job(id).output(...)`, `.send({value})`, and `.cancel()`.

### Saved job output

Tools execute once and capture their complete results to disk. File reads capture snapshots;
subsequent retrieval does not reread changed files. Search and glob capture their complete result
sets. There is no configured capture-size cap or automatic eviction; storage failures are reported
as failures, with retained partial output marked incomplete.

Model-facing responses include the job ID, tool, state, location, and a structured `result`.
Only output fields annotated with `x-skyhook-truncatable: true` may be shortened. Each annotated
field independently retains at most 100 lines or 2 KiB (2048 bytes), whichever is reached first.
Strings count UTF-8 content bytes before JSON escaping; arrays count their saved JSON text and
retain only complete items. All other fields remain intact regardless of size, so there is no
aggregate response-size limit or whole-result fallback.

Annotations cover file `content`, directory `entries`, process `stdout` and `stderr`, search
`matches`, glob `paths`, skill asset `content`, and shared `console` text. Skill instructions
remain complete. Shortened fields keep their original types;
`truncated: [{field, next}]` identifies each one and supplies a cursor starting at the remaining
content. `capture_complete` separately reports whether capture finished successfully. Errors
and questions are returned in full. Schemas are persisted with jobs so these rules also apply
after session resume and to completed remote jobs.

Full JavaScript tool results remain available for programmatic transformations. Arbitrary script
return values have no truncation annotations and are returned in full; script console text uses
the shared per-field limit. Explicit `job_output` selections return a `preview`, defaulting to
100 lines with bounded page content. Unannotated job metadata is always returned in full.

```js
// Read a selected part of a saved result.
job_output({job:42, field:"/result/stdout", start:300, limit:80})
// Search stored text, with source line numbers and surrounding context.
job_output({job:42, field:"/result/stderr", pattern:"(?i)error|warning", context:2})
// Resume an opaque saved cursor, optionally waiting for new output.
job_output({job:42, cursor:"...", wait:30})
```

`field` is a JSON Pointer: `/result/content` selects a file snapshot, `/result/stdout` and
`/result/stderr` select process streams, and `/console` selects script console text. Objects and
arrays have deterministic JSON text views. Long lines are split into UTF-8-safe fragments;
`line` and `offset` identify each fragment. Regex matching is case-sensitive unless inline flags override it. Matching supports lines up to 4 MiB and reports
an explicit resource error for larger lines; ordinary paging can still read those lines.

`limit` is 1–1000 lines, `start` is one-based, `context` is 0–20 surrounding lines, and `wait`
is 0–3600 seconds (default 0). A cursor retains its field and query: do not combine it with
`field`, `start`, `pattern`, or `context`; `limit` and `wait` can change. Cursors are repeatable
and survive session resume. `next` continues retained content; `capture_complete` separately
reports whether capture finished successfully. A wait timeout never stops the original job.

Local process output can be read while running. Remote shims capture first and transfer their
results in bounded frames after execution completes. Once transferred, output can be queried
without reconnecting to the remote machine.

The journal stores the exact model-visible previews, pages, and notifications. Full artifacts
are separate; provider-neutral request reconstruction reuses committed content rather than
regenerating it from current files or settings. Session format 1 has no migration layer.

Host-owned skills are exposed through `skills` and `skill`; they are discovered from the user
configuration directory and `.agents/skills` directories along the workspace ancestry.

## Development

```sh
cargo test -p skyhook-agent-core --all-targets
cargo test -p skyhook-agent --all-targets --no-default-features
cargo clippy --workspace --all-targets --no-default-features -- -D warnings
```

Build both statically linked Linux shims and the release CLI with
`cargo build --release -p skyhook-agent`. This default-feature build requires Cross and a running
container engine.

Skyhook is licensed under AGPL-3.0-only.

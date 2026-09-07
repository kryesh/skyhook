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
local-only build that does not require Cross is available with
`--no-default-features --features tui`; SSH use from that build returns an explicit missing-shim
error. The default `tui` feature enables the interactive binary and its UI dependencies;
`--no-default-features --features shim-bin` builds only the remote shim without those dependencies.

## Run

```sh
cp skyhook.example.toml ~/.config/skyhook/config.toml
export OPENAI_API_KEY=...
skyhook --prompt "inspect this repository"
```

Skyhook opens a full-screen terminal interface. `--prompt` submits an initial message;
`--script workflow.js` starts a JavaScript workflow in the same interface. Both remain open
for inspection and follow-up input after the work finishes. An interactive terminal is required;
there is no plain-output or redirected-input conversation mode.

Startup and `/new` open an empty draft without creating a session or its files. The session is
created when you send the first message or explicitly run a script; leaving an unused draft
behind does not create an empty saved session. Resuming an existing session still opens it immediately.

Use `--config path.toml` for explicit configuration, `--workspace PATH` for the workspace,
`--resume SESSION_ID` to reopen a session, and `-m/--model PROFILE` to choose a model for a new
session. `--image PATH` attaches an image to the initial `--prompt`. `--approve-all` or
`approve_all = true` bypasses approval prompts. Otherwise reads, agent operations, and writes
inside the root workspace are allowed; execution, remote access, target changes, and writes
outside the workspace require confirmation. Questions and SSH authentication appear in the
interface for every launch mode.

User messages appear on the right and assistant messages on the left. Tool previews show their
remote execution target after the tool name, such as `exec @lab-monitoring`; local calls omit `@root`. Tool calls expand inline
with named argument fields, nested lists, and syntax-highlighted scripts, commands, file content,
and diffs. JSON results and result pages are pretty-printed; source and plain-text log whitespace
is preserved. Light and dark themes each control the background and syntax colours; dark mode
uses a pure black background. All
formatting is local to the UI and leaves session records unchanged. Click an expanded body to
collapse it, or drag to select text. The agent tree appears above the composer while children
are active or a child agent is being viewed, with blank padding matching the input. Click an agent to inspect its conversation
without mixing its output with other agents. Each agent retains its reading position and expanded rows.
Agent tree rows show `@target` for non-root agents. Agent call previews show the child's target,
including while queued or running; an omitted target inherits the calling agent's target.
Agent rows show their own token totals and context usage in the same compact format as the
bottom-right session summary: output · input (uncached) · context. The State tab includes the
same per-agent summary when the terminal is too narrow to show it in the tree.
Status messages, including interruptions and errors, appear as distinct rows in the conversation
log and are saved with the session. They are excluded from the model’s context.
Completed final replies show their recorded model ID in a muted footer below the answer.
The composer always sends to the root agent. While root is busy, Enter queues a follow-up
for its next model request, without interrupting the current request or tools. It does not wait
for the entire turn to finish. `/queue` edits/removes input that has not yet been consumed,
and `/resume` resumes a queue paused by interruption.
`/retry` continues a failed or interrupted root turn without duplicating the original prompt.

The inspector provides Conversation, Requests, Jobs, and State tabs. Requests show the recorded
provider-neutral input, committed responses, usage, and compaction checkpoints, including retries. Job output
is paged and searchable without acknowledging the agent's pending notifications. Select a job
and press `o` for output fields, regex search, and the next page; `c` requests cancellation.
Remote output is available after transfer completes. Provider-supplied reasoning streams in a separate
expanded block with an animated spinner and collapses as soon as answer text starts (or the response
finishes). Single-line reasoning stays inline without an expand/collapse control and is not
selectable, even when it wraps in a narrow terminal. Reasoning uses the same Markdown rendering as replies. A separate working
spinner appears while a request is active without a reasoning spinner. Click a multi-line block or press Enter when selected to
reopen it, including after resuming a session. `/thinking` toggles expansion of saved reasoning.

### Model selection and UI state

Models are listed in configuration declaration order. For a new session, selection uses
`--model`, then the most recently submitted configured model, then the first model in the list.
The old `default_model_profile` configuration key is accepted but ignored. `/model` (or `Ctrl+X M`)
selects the model for subsequent user messages in the current session. Selection stays in the UI
until a message is sent; cancelling the picker or leaving without sending does not change the
session's recorded model. Each submitted message captures its model, including queued messages.
Queued messages are submitted together as one batch, in order, including their attachments.
The batch cannot be split across requests; the last message's captured model is used for that request.
Tool follow-ups, retries, compaction, and `/retry` retain the active turn's model. The bottom bar
shows the choice for the next message; reply footers identify the model that actually answered.
Resumed sessions retain their last applied model and instruction profile. Instruction-profile
changes still apply only to new sessions. `/models` remains an alias for `/model`.
Restore a missing recorded profile before resuming rather than substituting another model.

The interface stores the last submitted model and theme selection in `$XDG_STATE_HOME/skyhook/ui.json`, falling back
to `~/.local/state/skyhook/ui.json`. Writes are atomic and do not rewrite the model configuration.
Session titles are stored separately from conversation history in each session's `ui.json`.

The bottom bar uses this format:

```text
default · openai       8.4k · 200k(31.2k) · 42% (54k/128k)
```

Values are session output tokens, session total input tokens (uncached input), and the selected
agent's estimated current context occupancy (current tokens/model capacity). Session totals
include children and compaction. Context includes instructions, tools, history, and runtime
state; it is not cumulative usage. Missing context data appears as `—`.

### Keyboard and mouse

`Ctrl+X` is a leader: release it, then press the next key within two seconds.

| Shortcut | Action |
| --- | --- |
| `Ctrl+P`, `/` | Commands |
| `Ctrl+X N`, `Ctrl+X L` | New session, session picker |
| `Ctrl+X M`, `/model` | Model for subsequent user messages |
| `/profiles` | Instruction profile for new sessions |
| `Ctrl+X A`, `Ctrl+X I` | Agent picker, conversation inspector |
| `Ctrl+X S`, `/requests`, `/jobs` | State, requests, jobs |
| `Ctrl+X ↑`, `Ctrl+X ↓` | Parent, first child |
| `Ctrl+X T` | Dark/light theme |
| `Ctrl+X E` | Edit draft in `$EDITOR` |
| `Ctrl+X Y`, `Ctrl+X X` | Copy message/selection, export conversation |
| `Tab`, `Shift+Tab` | Focus composer, tree, content |
| `Enter` | Send/queue, select, expand |
| `Alt+Enter`, `Ctrl+J`, supported `Shift+Enter` | Newline |
| `PageUp`, `PageDown` | Scroll history |
| `Ctrl+Alt+U`, `Ctrl+Alt+D` | Half-page scrolling |
| `Home`, `End` in content | Beginning, latest |
| `/`, `n`, `N` in content | Search, next/previous match |
| `[`, `]` in content | Previous/next inspector tab |
| `Esc` | Dismiss local interaction or interrupt work |
| `Ctrl+C` | Clear draft, otherwise interrupt/quit |
| `Ctrl+X Q` | Quit |

Menus use arrows, the mouse wheel, or `Ctrl+P/N`; Enter or Tab selects. Theme choices
preview immediately; Escape restores the previous theme and Enter saves the choice. The composer supports word movement,
selection, `Ctrl+A/E`, `Ctrl+W`, `Ctrl+U/K`, and undo/redo with `Ctrl+-` / `Ctrl+.`.
Click agent and tool rows, scroll the relevant panel, or drag across text in user/agent messages
and tool output, then copy the selected characters with `Ctrl+X Y` (or `y` while content is focused).
Selection supports parts of a line and multiple lines; copying preserves Unicode and code indentation
without adding newlines at visual wraps.
The workspace path and session ID in the top bar are plain text; use the terminal emulator’s
selection gesture (usually Shift-drag) and copy shortcut. The bottom bar shows the model ID
and token statistics. Copy uses the terminal's OSC 52 clipboard support. `@` attaches a workspace file; `/attach`
adds an image. Large pastes appear as attachments; click their chips or use `/attachments` to
inspect or remove them. Questions and permissions open even while inspecting the agent tree or
conversation; open menus and search keep input focus until closed. Dismissed requests can be
reopened with `/attention`. SSH authentication/askpass prompts take priority over questions,
permissions, menus, and search; interrupted question drafts resume afterward. `/diagnostics`
lists startup warnings such as skipped skills.

Within questions and permissions, `↑`/`↓` selects an answer, `PageUp`/`PageDown` scrolls
the prompt text, and `Ctrl+PageUp`/`Ctrl+PageDown` scrolls long answer descriptions.
The mouse wheel scrolls the text or choices beneath the pointer. Drafts remain intact while
answering questions or inspecting details.
Question choices are suggestions: you can select one, optionally add a comment, or provide
a free-form answer. A suggestion without a non-whitespace comment returns its label as a string;
with a comment it returns `{"answer": "selected label", "comment": "user text"}`. Free-form
answers remain strings.

Optional settings live in `$XDG_CONFIG_HOME/skyhook/tui.toml` (or `~/.config/skyhook/tui.toml`):

```toml
theme = "dark"

[keybinds]
model = "ctrl+x m"
inspect = "ctrl+x i"
# Disable an action binding with "none". /help lists available actions.
```

### Host observation API

`Config::harness_builder(workspace, model)` takes an explicit model choice from its host.
`SessionHandle::observe()` returns an atomic snapshot/receiver pair with revisioned updates,
request-scoped live responses, current activity, and context estimates. On receiver lag, replace
both with a fresh observation. Durable records are identified by their original sequence.
`inspect_jobs` and `inspect_output` inspect metadata and saved output without claiming jobs or
consuming notifications; `cancel_job` explicitly requests cancellation. `SessionStore::read_records`
reads an archive without acquiring a writer lock or repairing a partial final line.

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
independently of reasoning settings. Built-in Flux adapters transmit it through OpenAI Chat
Completions, Responses, and Anthropic Messages formats. Codex OAuth schema calls use HTTP while
ordinary calls retain Flux's WebSocket transport. Models/endpoints must support structured output;
an opaque backend factory wrapped with `FluxProvider::new` rejects schemas explicitly instead of ignoring
them. Ordinary agent requests have no response schema.

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

- `provider::Provider` is a shared factory: `open_context(correlation)` creates an owned
  `ProviderContext`, whose `invoke(&mut self, request)` returns an asynchronous response stream.
  Adapters live under `provider::backends` and wire types under `provider::protocol`.
  Custom providers implement both traits; `FluxProvider::new` takes a closure that creates a
  fresh backend per context. A request's correlation must match its context's identity.
- Each agent loop owns an `AgentContext`: projected journal history, model profile and request
  template, token accounting, and its provider handle. Codex contexts have separate WebSocket
  connection slots and ordinary/schema HTTP routing state, with shared authentication tokens.
  Handles survive turns, retries, questions, and compaction, and are released when the agent exits.
  Model-profile changes prepare a replacement before committing, preserve history, and reset token
  calibration. Equivalent profiles retain their handle. Resuming opens a fresh handle with the
  same agent cache identity; compaction clears calibration from the previous history projection.
- `tool::ToolRegistryBuilder` supports typed and JSON-based tools, while `tool::executor::ToolExecutor`
  turns every invocation into a supervised job. Typed registrations generate both input and output
  schemas; compact result shapes are included in model and script documentation.
- `session::SessionStore` persists a versioned append-only JSONL log, content-addressed image blobs, job
  outputs, and line-addressable job output.
- `agent::Harness` owns profiles and policy; each `agent::SessionHandle` owns an isolated agent tree
  and registry.
- Child agents are one-shot, profile-selectable agents. Each model-facing `ask` contains one
  `{id, prompt, options}` question; independent concurrent calls are merged by the runtime. A child
  question batch changes its stable agent job to `waiting_input`; answer that job with
  `tool.job(id).send({value: answer})` in a script. For a merged batch, use
  `tool.job(id).send({value: {question_id: answer, another_id: answer}})`; the keys are the IDs
  included in the waiting job's `questions` output. Each answer can be a string (a suggestion
  label or free-form text), or `{"answer": "selected label", "comment": "user text"}` for a
  suggestion with a non-whitespace comment. A single question returns that value directly;
  a merged batch keeps each value under its question ID.
  Root-agent questions still go directly to the host question handler.

`agent` accepts an optional `depth` delegation budget. It defaults to zero, making the launched
child a leaf. A caller may grant less than its own available depth; once no depth remains, `agent`
is omitted from both model tools and script bindings. Its optional `workspace` accepts relative or
absolute directories for both local and remote children. Relative overrides resolve against the
workspace selected by the target rules. Children receive a fresh conversation, shared harness
instructions and host-owned skills, and their parent's active model, including model switches and
restored session selections. An explicit child `model` takes precedence over the model in an
explicitly selected `profile`; otherwise the child inherits its parent's model. The harness default
agent profile still supplies instructions when `profile` is omitted, without changing the inherited
model. This applies equally to local children, remote children, and deeper descendants. Agents on
the same target share files; a workspace override creates no filesystem isolation. Children must
finish or cancel all owned jobs and descendants before their agent job completes. Only root agents
may leave background services running after answering.

Filesystem tools and command `cwd` accept absolute paths or relative paths including `..`.
The workspace is a base directory, not a security boundary. Canonical paths outside the configured
workspace require approval, cached in memory by session, target, access mode, and path; directory
approval covers descendants. Read and write grants are separate. Process execution remains subject
to confirmation on each invocation.

Background-capable tools accept an optional `bg` argument. Model-facing calls return a job view
with `id`, `state`, the actual `target` when enabled, and `result`. Workspace is included when it differs from the caller. Listings and notifications also include tool identity and applicable name/parent metadata. Only annotated fields are shortened; `truncated` lists their total line counts and next read positions.
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
lowercase kebab-case: start with a letter, then use lowercase ASCII letters, digits, and single
hyphens between nonempty words. Examples include `inspect-config`, `run-tests`, and `build-v2`.
Names are descriptive labels, do not need to be unique, and do not replace job IDs.

```js
return tool.exec({argv: ["cargo", "test"], name: "run-tests", bg: true});
```

Names appear in active-job state, job envelopes (including notifications and inspection), and
durable job records. They survive session resume. Omitted or null names leave jobs unnamed;
invalid names are rejected before execution. JavaScript builders also support `.name("run-tests")`.

## Todos and runtime context

Every agent has an ordered advisory todo list. `todo()` reads the caller's list;
`todo({items:[...]})` replaces the whole list and returns `{updated:true}`; an empty array clears it. Each item has
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
`{items}`. An agent without todos has an empty list. Inside the child, progress can be
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
    "name": "run-tests",
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

Search and glob patterns filter eligible files without overriding hidden-file or ignore settings.
Tool-owned null metadata is omitted; literal nulls inside file contents, script returns, or user JSON are preserved.
Process results omit empty streams and false timeout flags, keeping exit code zero and nonempty stderr.
Completed agent calls return their complete answer string without automatic truncation. Child questions return `{questions:[{id,prompt,options?}]}`.

Directory reads return grouped entries with file sizes, for example:
`{kind:"directory",path:"src",entries:{files:[{name:"main.rs",bytes:4096}],directories:["lib"]}}`.
Groups are `files`, `directories`, `symlinks`, and `other`, with sorted names and empty groups omitted.
`read({path:"src",details:true})` returns flat `{name,kind,bytes?}` entries; regular files include sizes in both forms.
Search returns `{matches:{"src/main.rs":["12: matching text"]}}`, preserving source whitespace.
`search({pattern:"...",details:true})` returns structured `{path,line,column,text}` matches instead.
Empty compact directory/search maps are `{}`. Grouped maps share a single normal preview budget.
`targets({details:true})` returns full target metadata without nulls; defaults and `target_add` use compact
name/type/host records with nondefault origin/workspace and configured via where applicable.
All defaulted input fields, including `details: false`, are optional in tool schemas.

By default, hidden entries (including `.git`) and ignored files are excluded. `hidden: true`
includes hidden entries, while `no_ignore: true` independently disables ignore files. Searches
rooted in subdirectories inherit ancestor ignore rules; explicitly requested paths remain accessible.

Model-facing direct responses include the job ID, state, applicable target/workspace, and `result`.
Only output fields annotated with `x-skyhook-truncatable: true` may be shortened. Each annotated
field independently retains at most 100 lines or 2 KiB (2048 bytes), whichever is reached first.
Strings count UTF-8 content bytes before JSON escaping; arrays and grouped maps count their saved JSON text and
retain only complete items. Grouped maps share one budget across all groups. All other fields remain intact regardless of size, so there is no
aggregate response-size limit or whole-result fallback.

Annotations cover file `content`, directory `entries`, process `stdout` and `stderr`, search
`matches`, glob `paths`, skill asset `content`, and shared `console` text. Skill instructions
remain complete. Shortened fields keep their original types;
`truncated: [{field, total_lines, next_start, next_offset?}]` identifies each one, reports its
total source lines, and supplies the exact first unread position. Finished jobs with an incomplete capture include an `Output incomplete.` notice. Errors
and questions are returned in full. Schemas are persisted with jobs so these rules also apply
after session resume and to completed remote jobs.

Full JavaScript tool results remain available for programmatic transformations. When a script
returns an unchanged tool-result object or array, it is presented as that child's native job view,
wherever it appears in the return structure. The view replaces the raw tool result and carries
the child job ID, bounded annotated fields, and child read positions. A script still produces
one tool response; custom objects and array ordering are preserved.

Edited tool results remain script-owned data with their original field annotations. Extracted
original arrays and grouped maps retain annotations; extracted primitive strings and newly constructed data do not.
Their unannotated content remains complete. Script-owned truncation markers and console text use
the script job ID; child-view read positions use the child job ID. Presentation never changes the full
saved return value. Default script output retrieval reproduces the composed views; explicit field
selections read the saved script data. Background handles and existing job views are not wrapped
again. Logging and returning the same data explicitly produces both outputs.

Script console text uses the shared per-field limit. Explicit `job_output` selections return a `preview`, defaulting to
100 lines with bounded page content. Unannotated job metadata is always returned in full.

```js
// Read a selected part of a saved result.
job_output({job:42, field:"/result/stdout", start:300, limit:80})
// Search stored text, with surrounding context.
job_output({job:42, field:"/result/stderr", pattern:"(?i)error|warning", context:2})
// Continue at the returned source line and UTF-8 byte offset.
job_output({job:42, field:"/result/stdout", start:22, offset:54, limit:100, wait:30})
```

`field` is a JSON Pointer: `/result/content` selects a file snapshot, `/result/stdout` and
`/result/stderr` select process streams, and `/console` selects script console text. Objects and
arrays have deterministic JSON text views. Long lines are split into UTF-8-safe fragments;
the next position identifies where to continue. Regex matching is case-sensitive unless inline
flags override it. Matching supports lines up to 4 MiB and reports an explicit resource error
for larger lines; ordinary paging can still read those lines.

`start` is one-based (default 1); `offset` is a zero-based UTF-8 byte offset within that
starting line (default 0). `limit` is 1–1000 returned source lines (default 100), including
match context. `context` is 0–20 surrounding lines (default 0); positive context requires `pattern`; `wait` is 0–3600
seconds (default 0). Every argument with a default is optional in the tool schema.

Explicit read pages contain `field` and `lines`, plus `total_lines` when known and a next position when more content may be available.
`lines` is an array of strings, one per returned line (or fragment of an oversized line),
without per-line objects or match flags. Empty fields have zero lines;
a final unterminated line counts, and a trailing newline does not add an empty line.
Read pages and automatic string previews prefer whole lines; oversized lines are split at
UTF-8 boundaries. The page byte budget may return fewer lines than `limit`: use the returned
position instead of computing `start + limit`. Offsets beyond a line or inside a UTF-8
character are rejected. A start past the available lines returns an empty page with the total.

For closed fields, an omitted `next_start` means no selected content remains. Omitted `next_offset` means zero.
For running fields, the total describes currently captured output and the numeric next position
can be retried with `wait`, even when no content is currently available. Unavailable output omits
`total_lines`; known empty output retains zero. Repeat the field, regex, and context when continuing a search; overlapping
match context is reconstructed from saved text. Live searches defer incomplete lines and context
windows until more output arrives or capture closes. A wait timeout never stops the original job.

Reads are repeatable and survive session resume, without opaque tokens or saved query state.
The old `cursor` argument is no longer accepted. Capture completeness remains internal; finished
jobs with retained partial output have an `Output incomplete.` notice.

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
cargo test -p skyhook-agent --all-targets --no-default-features --features tui
cargo clippy --workspace --all-targets --no-default-features --features tui -- -D warnings
```

Build both statically linked Linux shims and the release CLI with
`cargo build --release -p skyhook-agent`. This default-feature build requires Cross and a running
container engine.

Skyhook is licensed under AGPL-3.0-only.

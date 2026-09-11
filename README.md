# Skyhook

Skyhook is a provider-neutral coding-agent harness with a programmable JavaScript orchestration
runtime. A tool is registered once and is then available through the model tool protocol and as a
lazy builder inside `script`.

The workspace contains the source-only `skyhook-agent-core` library package (whose Rust crate is
named `skyhook`) and the installable `skyhook-agent` CLI package. The core does not depend on a
provider-specific response type; native backends implement OpenAI Chat Completions and Responses,
Anthropic Messages, and Codex/ChatGPT subscription behind a common provider API.

## Install

Rust 1.88 or newer is required. Default installs build static Linux SSH shims from source, so
install [Zig](https://ziglang.org/download/) and the Rust musl targets first. Make `zig` available
on `PATH` (or set `CARGO_ZIGBUILD_ZIG_PATH` to its executable):

```sh
zig version
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
cargo install skyhook-agent --locked
```

`build.rs` uses the `cargo-zigbuild` Rust library with your installed Zig; you do not need to
install the `cargo zigbuild` CLI, Cross, Docker, or Podman.

Install directly from Git or a local checkout with:

```sh
cargo install --git https://git.kryesh.tech/Apps/skyhook.git skyhook-agent --locked
cargo install --path . --locked
```

The `linux-ssh` binary is built from `src/bin/linux-ssh` for these shim targets:

| Platform | Protocol | Architecture | Rust target |
| --- | --- | --- | --- |
| `linux` | `ssh` | `x86_64` | `x86_64-unknown-linux-musl` |
| `linux` | `ssh` | `aarch64` | `aarch64-unknown-linux-musl` |

The resulting static ELF executables are validated, written to the gitignored generated-artifact
folder as `target/shims/linux-ssh-x86_64` and `target/shims/linux-ssh-aarch64`, and embedded in the installed
`skyhook` command with `rust-embed`. Git and the Cargo source package contain source only, not
prebuilt shim binaries.

For a local-only install without Zig or the extra musl targets:

```sh
cargo install skyhook-agent --locked --no-default-features --features tui
# Or from a local checkout:
cargo install --path . --locked --no-default-features --features tui
```

SSH use from that build returns an explicit missing-shim error. The default `tui` feature enables
the interactive binary and its UI dependencies. To build only the Linux SSH shim for the native
host target, without Zig or the UI dependencies, use
`cargo build --bin linux-ssh --no-default-features --features shim-bin` on Linux.

## Run

```sh
cp skyhook.example.toml ~/.config/skyhook/config.toml
export OPENAI_API_KEY=...
skyhook --prompt "inspect this repository"
```

Skyhook opens a full-screen terminal interface. `--prompt` submits an initial message;
`--script workflow.js` starts a JavaScript workflow in the same interface. Both remain open
for inspection and follow-up input after the work finishes. These interface modes require an
interactive terminal. Add `--non-interactive` to `-p/--prompt` or `-s/--script` for headless execution
with redirected input/output support and automatic exit; see [Headless execution](#headless-execution).

Startup and `/new` open an empty draft without creating a session or its files. The session is
created when you send the first message or explicitly run a script; leaving an unused draft
behind does not create an empty saved session. Resuming an existing session still opens it immediately.

Use `--config path.toml` for explicit configuration, `--workspace PATH` for the workspace,
`--resume SESSION_ID` to reopen a session, and `-m/--model PROFILE` to choose a model for a new
session. `--image PATH` attaches an image to the initial `--prompt`. `--approve-all` or
`approve_all = true` bypasses approval prompts. Otherwise reads, agent operations, and writes
inside the root workspace are allowed; execution, remote access, target changes, and writes
outside the workspace require confirmation. When the `interactive` capability is granted, questions
and SSH authentication appear in the interface. Otherwise operations needing human input fail
immediately; `approve_all` bypasses tool approvals but never restores questions or authentication.

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
bottom-right session summary: output · input (uncached) · context.
Status messages, including interruptions and errors, appear as distinct rows in the conversation
log and are saved with the session. They are excluded from the model’s context.
Completed final replies show their recorded model ID in a muted footer below the answer.
The composer always sends to the root agent. While root is busy, Enter queues a follow-up
for its next model request, without interrupting the current request or tools. It does not wait
for the entire turn to finish. `/queue` edits/removes input that has not yet been consumed,
and `/resume` resumes a queue paused by interruption.
`/retry` continues a failed or interrupted root turn without duplicating the original prompt.

The inspector provides Conversation, Requests, and Jobs tabs. Requests show one metadata row per
model call, including retries and compaction calls, with token summaries alongside the row. Full
request bodies remain in the session journal for reconstruction but are not rendered in the UI.
Startup warnings appear directly in the conversation log rather than a separate diagnostics page.
Job output is paged and searchable without acknowledging the agent's pending notifications. Select a job
and press `o` for output fields, regex search, and the next page; `c` requests cancellation.
Intermediate child-agent messages and terminal job notifications appear as expandable **Job event**
cards in the conversation. Agent-message cards show the job identity/name, source message sequence,
and readable progress text when expanded, rather than raw runtime envelopes. They retain their
historical payload when the job later completes or is resumed; ordinary user text is not reclassified.
This presentation also applies when reopening older saved sessions.
Remote output is available after transfer completes. Provider-supplied reasoning streams in a separate
expanded block with an animated spinner and collapses as soon as answer text starts (or the response
finishes). Single-line reasoning stays inline without an expand/collapse control and is not
selectable, even when it wraps in a narrow terminal. Reasoning uses the same Markdown rendering as replies. A separate working
spinner appears while a request is active without a reasoning spinner. Click a multi-line block or press Enter when selected to
reopen it, including after resuming a session. `/thinking` toggles expansion of saved reasoning.

### Model selection and UI state

Models are listed in configuration declaration order. For a new session, selection uses
`--model`, then the most recently submitted configured model, then the first model in the list.
`/model` (or `Ctrl+X M`)
selects the model for subsequent user messages in the current session. Selection stays in the UI
until a message is sent; cancelling the picker or leaving without sending does not change the
session's recorded model. Each submitted message captures its model, including queued messages.
Queued messages are submitted together as one batch, in order, including their attachments.
The batch cannot be split across requests; the last message's captured model is used for that request.
Tool follow-ups, retries, compaction, and `/retry` retain the active turn's model. The bottom bar
shows the choice for the next message; reply footers identify the model that actually answered.
Resumed sessions retain their last applied model. `/models` remains an alias for `/model`.
Restore a missing recorded model profile before resuming rather than substituting another model.

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
| `Ctrl+X A`, `Ctrl+X I` | Agent picker, focus conversation |
| `/requests`, `/jobs` | Requests, jobs |
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

The command palette omits navigation-only actions; `Ctrl+X I`, `Ctrl+X ↑`, and `Ctrl+X ↓`
remain available to focus the conversation, select the parent, and select the first child.
Menus use arrows, mouse hover, the mouse wheel, or `Ctrl+P/N`; Enter or Tab activates
the selected item. Open palettes isolate hover from the conversation underneath. The Agents
palette labels its Output, Input (uncached), and Context statistics. Theme choices preview
immediately; Escape restores the previous theme and Enter saves the choice. The composer
wraps at word boundaries and supports word movement, selection, `Ctrl+A/E`, `Ctrl+W`,
`Ctrl+U/K`, and undo/redo with `Ctrl+-` / `Ctrl+.`. Up/Down moves through displayed input
rows, reaching prompt history only from the first/last row.
Click agent and tool rows, scroll the relevant panel, or drag across text in user/agent messages
and tool output, then copy the selected characters with `Ctrl+X Y` (or `y` while content is focused).
Selection supports parts of a line and multiple lines; copying preserves Unicode and code indentation
without adding newlines at visual wraps.
The workspace path and session ID in the top bar are plain text; use the terminal emulator’s
selection gesture (usually Shift-drag) and copy shortcut. The bottom bar shows the model ID
and token statistics. Copy uses the terminal's OSC 52 clipboard support. `@` attaches a workspace file; `/attach`
adds an image. Pastes longer than 12 lines and attached file contents appear as inline items
at the cursor. Type before, between, or after multiple paste items; move across, select, delete,
and undo them as single editing units. Sending or copying expands their original contents in
place. Shorter pastes remain ordinary editable text. Use `/attachments` to inspect or remove
paste items and images; `$EDITOR` opens the fully expanded draft for editing.
Questions and permissions open even while inspecting the agent tree or
conversation; open menus and search keep input focus until closed. Dismissed requests can be
reopened with `/attention`. SSH authentication/askpass prompts take priority over questions,
permissions, menus, and search; interrupted question drafts resume afterward. `/diagnostics`
lists startup warnings such as skipped skills.

Within questions and permissions, `↑`/`↓` selects an answer, `PageUp`/`PageDown` scrolls
the prompt text, and `Ctrl+PageUp`/`Ctrl+PageDown` scrolls long answer descriptions.
In multi-question prompts, `←`/`→` switches questions while navigating choices, preserving
each question's selected option and unfinished text. Typing enters text-editing mode, where
`←`/`→` moves the cursor; `Tab` switches between editing and question navigation. `Enter`
confirms an answer and advances to an unanswered question; switching alone never submits.
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

## Headless execution

Use `--non-interactive` with exactly one initial prompt or script:

```sh
session_id=$(skyhook --non-interactive -p "Review this repository" --capabilities read,agents)
skyhook --non-interactive -s workflow.js --approve-all
skyhook --non-interactive --resume "$session_id" -p "Summarize the findings"
```

Headless mode does not require a terminal, read answers from stdin, or load TUI themes/keybindings.
It creates or opens the session, prints **only its session ID followed by a newline to stdout**, and
flushes that line before executing the prompt or workflow. It then exits automatically. The ID lets
external programs locate and follow the normal session logs under the configured `session_root`
(default: `<workspace>/.skyhook/sessions`). Resumed runs print the existing session ID.

Assistant output, script console output/results, startup warnings, and execution diagnostics remain
in the session logs; they are not printed to stdout or stderr. The process exits successfully when
the submitted operation and shutdown succeed, and nonzero on failure or interruption. A failure
before a session can be opened produces no session-ID line. Explicit `--help` and `--version`
retain their normal output. Headless mode is not a line-oriented conversation over stdin.

The submitted root turn or workflow defines completion. A workflow must explicitly await background
work it needs completed; outstanding jobs are cancelled and drained during shutdown. Interrupt and
termination signals also trigger cleanup and journaled status rather than a terminal prompt.

`--non-interactive` always disables human interaction. Root `ask` is unavailable (also inside scripts and with `bg:true`). Child agents
can still ask their owning parent agent. Operations requiring human approval fail immediately;
ordinary automatically allowed operations still work. `--approve-all` (or config `approve_all = true`)
bypasses tool approvals but does not enable questions, SSH passwords/passphrases, or host/agent
confirmation prompts. SSH credentials that work without a prompt can still authenticate.

Without `interactive`, `exec` and `shell` run in a new process session with no controlling terminal,
so ordinary `/dev/tty` prompts cannot stop the job waiting for terminal input. Each command forces a
Skyhook-owned rejecting askpass helper over inherited `SSH_ASKPASS` settings, even when `targets` is
disabled. Existing `SSH_AUTH_SOCK` credentials are preserved unless the configured target-authentication
setup replaces them. Remote tool requests carry the caller's exact capabilities, so remote commands
apply the same restrictions; incompatible shim protocol versions are rejected rather than falling
back to default capabilities. This is still not an OS sandbox against deliberately programmed
subprocesses that establish their own external interaction mechanisms.

Askpass sockets and helpers live in uniquely created `skyhook-askpass-<pid>-<random>` temporary
directories. No fixed socket pathname is shared across Skyhook instances, or even across concurrent
askpass servers in one instance. Directories and helpers have explicit `0700` permissions and sockets
have `0600` permissions independent of umask; the listener accepts only peers with the same effective
UID. Each owner removes only its own socket/helper/directory during cleanup.

## Capabilities

The top-level configuration has one exact policy-capability allowlist:

```toml
# These are the defaults when capabilities is omitted. Add "targets" to enable targets.
capabilities = ["read", "write", "exec", "network", "agents", "mcp"]
```

| Capability | Controls | Default |
| --- | --- | --- |
| `read` | File reads, searches, and skill reads | Enabled |
| `write` | File creation, modification, removal, and related write access | Enabled |
| `exec` | `exec` and `shell` | Enabled |
| `network` | HTTP(S) `fetch` | Enabled |
| `targets` | Target-management tools and target-selection inputs | Disabled |
| `agents` | Child-agent creation, also limited by available delegation depth | Enabled |
| `interactive` (runtime only) | Human-facing root questions, approval prompts, and sensitive authentication | Enabled by the terminal host; disabled in headless mode |
| `mcp` | MCP server startup and MCP tool availability | Enabled |

An explicit array replaces the defaults; `capabilities = []` grants no policy capabilities.
Unknown names and `interactive` are errors in this list and in `--capabilities`.
`--capabilities read,write` replaces the config allowlist for that invocation; `--capabilities=`
selects an empty policy set. Resolution is **CLI allowlist > config allowlist > defaults**.
The host then supplies `interactive` according to the runtime mode, independently of the allowlist,
and per-agent depth restrictions narrow the result. Use `--non-interactive` to disable human
interaction; an empty allowlist does not disable the terminal interface or root questions.
Overrides also apply when resuming or creating another session in the interface. Enable target
support by including `"targets"` in the allowlist.

Capabilities gate both tool discovery and invocation, including JavaScript. Approval policies and
cached grants cannot restore a missing capability. Ungated orchestration tools remain available even
with an empty set. These gates are **not an OS sandbox**: allowing `exec` lets a command access files
or the network independently of the corresponding built-in tool gates. Disabling `network` removes
`fetch`, not model-provider traffic or trusted MCP transport setup; disable `mcp` to prevent MCP
connections. Per-server MCP requirements are additional gates, not grants.

## Reconstructing model calls

Session format 1 records the inputs needed to reconstruct each call at the shared `Provider`
boundary. It stores no backend-specific request bodies or authentication headers:

- `model_context` records the configured provider name and a shared `ModelRequest` template:
  actual model ID, assembled system prompt (including harness instructions and location),
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
`ModelRequest` for a `model_requested` sequence, using sequence-ordered records from `SessionStore`.
Image metadata references the existing session blobs;
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
independently of reasoning settings. Native backends transmit it through OpenAI Chat Completions,
Responses, and Anthropic Messages formats. Codex uses the same Responses codec for WebSocket and
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
the original history. Oversized estimates never cause preserved state to be dropped. Ordinary model
calls and summarization each allow up to three attempts total, including failures during streaming;
invalid summaries, truncated summaries, or summary tool-call responses are also retried. Recognized provider context-overflow errors force compaction
before the next ordinary attempt. Each attempt is journaled with its exact input, and failed streamed
tool calls are never executed. The CLI reports compaction start, estimated input reduction, skipped
compactions, failures, and ordinary request retries without printing the summary.

## Execution targets

Include `"targets"` in the top-level `capabilities` array (or CLI `--capabilities` allowlist) to
grant the session's target capability. It is not enabled by default. Without it, target-management
tools, target arguments, JavaScript target setters, and target prompt guidance are all omitted
from the model-visible surface. The former `targets_enabled` setting is no longer accepted.

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
origin into this central agent. Remote Skyhook commands receive OpenSSH’s private forwarded socket in
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

The CLI injects its embedded shim catalog into the core harness. Shims are selected by protocol,
platform, and architecture; artifact names follow `platform-protocol-arch` (for example,
`linux-ssh-aarch64`). The naming scheme allows an optional `.exe` suffix for future platforms,
but no Windows transport is implemented. Library embedders receive an empty catalog by default
and can provide their own `EmbeddedShimCatalog` through `HarnessBuilder`; selecting a combination
without a supplied shim returns an unsupported-platform error.

## Configuration

Providers and models are separate named profiles. API secrets can be read from environment variables
or retrieved lazily by a command; no literal API-key field is supported in TOML. `openai` requires
an explicit `base_url` and `api` (`chat_completions` or `responses`); `anthropic` requires an explicit
`base_url`. Both accept either `api_key_env` or `api_key_command`, or neither for a keyless endpoint.
URLs name the API root: Skyhook appends
`/chat/completions`, `/responses`, or `/messages`. For the official services use
`https://api.openai.com/v1` or `https://api.anthropic.com/v1`. There are no vendor presets, model
aliases, or automatic vendor detection. Use full model identifiers. A shared Chat codec normalizes
specific compatible streaming variations; backend wire types and replay policies stay behind the
unified `Provider` / `ProviderContext` interface.

```toml
approve_all = false
capabilities = ["read", "write", "exec", "network", "agents", "mcp"]

[providers.local]
kind = "openai"
base_url = "http://127.0.0.1:11434/v1"
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
points. Both must be positive and `max_output` must be smaller than `max_context`. Protocol-specific
limits are validated by the backend or service. Codex subscription does not accept an output-token
limit on the wire; its configured limit remains available to local context budgeting.

### API keys and environment files

Reference an environment variable with `api_key_env`, or use a command to retrieve the key:

```toml
[providers.anthropic]
kind = "anthropic"
base_url = "https://api.anthropic.com/v1"
api_key_env = "ANTHROPIC_API_KEY"
# Alternatively, remove api_key_env and use:
# api_key_command = "op read 'op://Private/Anthropic/api-key'"
```

`api_key_env` and `api_key_command` are mutually exclusive. Environment-variable keys are resolved
when the provider is built and must be present and nonblank. Commands are run only on the provider's
first model request, not when configuration is loaded or a conversation is opened. A successful
command's stdout is decoded as UTF-8 and trimmed of leading/trailing whitespace and newlines, then
cached in memory for that provider instance. Concurrent requests and child conversations share the
cache; a new process or provider instance resolves the key again. Failed commands are not cached
and may be retried on a subsequent request. Empty output, invalid UTF-8, invalid credential headers,
and nonzero exits fail the request without including command output in the error. Stdout is limited
to 64 KiB. Commands run before the HTTP startup timeout; there is no separate command timeout.
Cancelling the invocation terminates the command's immediate child process, but does not guarantee
termination of any descendants it launched.

Commands are trusted host configuration, executed with `/bin/sh -c`, inheriting Skyhook's process
working directory and environment, not an agent's workspace or remote target. They do not run
through tool approval. Standard input is closed and standard error is discarded; use a noninteractive
secret-manager command that writes only the key to stdout. Do not put literal secrets in command
strings or commit them to configuration files.

At CLI startup, Skyhook loads **`.env` in the invocation directory** before constructing providers.
Existing process environment variables take precedence. A missing file is ignored; Skyhook does
not search parent directories, the `--workspace` directory, or the configuration file's directory.
Loaded variables are also inherited by API-key commands and other **local** child processes. Neither
inherited host environment variables nor `.env` values are automatically forwarded into remote target
processes; remote commands use the remote machine's environment. Managed SSH routes disable `SendEnv`,
`SetEnv`, and X11 forwarding. Local SSH authentication/proxy helpers still use the host environment;
Skyhook's deliberate SSH-agent forwarding remains supported separately. Library embedders manage
their own process environment; loading a core `Config` does not load `.env`.

For example, in the directory from which you run `skyhook`:

```dotenv
ANTHROPIC_API_KEY="your-key"
```

Keep `.env` out of version control and restrict its file permissions. The CLI supports normal dotenv
quoting, comments, `export` declarations, and variable interpolation. A malformed or unreadable file
fails startup without printing its contents; non-interactive startup errors remain silent.

### MCP servers

Configure Model Context Protocol servers in the top-level **`mcp`** map, with one named
`[mcp.<name>]` table per server. The map defaults to empty: no MCP connections, tools, or MCP prompt
guidance are added unless servers are configured. Skyhook supports **stdio** and **Streamable HTTP**;
legacy HTTP+SSE transport is not supported.

```toml
[mcp.files]
transport = "stdio"
start_command = ["my-mcp-server", "--root", "/srv/project"]
capabilities = ["read"]
cwd = "mcp-work"                    # Relative to the selected config file's directory.
env = { LOG_LEVEL = "warn" }       # Environment overrides for the launched process.
startup_timeout_secs = 30           # Default; must be a positive integer.
call_timeout_secs = 120             # Default; must be a positive integer.

[mcp.service]
transport = "streamable_http"
url = "http://127.0.0.1:8080/mcp"    # Full MCP endpoint, not an API root.
capabilities = ["read", "write"]
headers_env = { Authorization = "MY_MCP_AUTHORIZATION" }
# Optional local startup fallback, only when the endpoint is unreachable:
# start_command = ["my-http-mcp-server", "--port", "8080"]
```

`start_command` is an argument vector, not shell text; its first element must name a nonempty
executable. Stdio requires it and rejects `url` and `headers_env`. Streamable HTTP requires an
absolute HTTP(S) `url`. It connects first, and only starts the optional command if the endpoint is
unreachable—not on authentication, protocol, or other server errors. `cwd` and `env` apply only to
`start_command`; HTTP connections without a startup command must omit them. Relative `cwd` is
resolved against the selected configuration file's directory, not the agent workspace. Unknown
fields, transports, capabilities, invalid transport combinations, and zero, negative, fractional,
or overflowing timeouts are rejected.

HTTP `headers_env` maps header names to environment-variable names. The variable's complete value
is sent as the header (for example, `MY_MCP_AUTHORIZATION` can contain `Bearer ...`); secrets do not
belong in TOML. Missing or non-Unicode variables, invalid header names, and values that cannot be
encoded as HTTP headers prevent that server's connection from starting. Empty header values are
passed through to the server. Do not put secrets in `start_command` arguments or literal `env` values.

**Trust boundary:** configured startup commands are trusted user configuration. They run during
harness startup without a model tool-approval prompt, only on the local root/session host—not on
selected SSH targets. Only configure commands and endpoints you trust. MCP clients and launched
processes are owned by the root session rather than independently restarted for each child agent.

The session must hold the global **`mcp` capability** before any MCP server is launched or contacted,
and before any MCP tool can be exposed or invoked. Each server's `capabilities` array defaults to
`[]` and adds requirements to that global gate. Valid names are `read`, `write`, `exec`, `network`,
`targets`, `agents`, `interactive`, and `mcp`. An agent must hold **all** listed capabilities to see
or call a server's tools; otherwise those tools are hidden from both the model catalog and
JavaScript access. Servers whose requirements exceed the session's capabilities are not contacted
or launched at all. This is a host-configured gate, not an inference from server annotations.
Nonempty lists also request those permissions through the normal approval policy, scoped to the
MCP server/tool rather than a filesystem workspace. The implicit global `mcp` requirement is
availability-only and introduces no approval prompt. `approve_all` never bypasses missing
capabilities. **An omitted or empty per-server list requires only global `mcp` and produces no
permission prompt.** MCP transports do not implicitly require `exec` or `network`; startup
commands and endpoints remain trusted host configuration.

Skyhook imports **tools only**, not MCP prompts or resources. At startup it initializes each server
and discovers its tools, then freezes that catalog for the session lifetime; later catalog-change
notifications do not add or replace tools. A server that fails startup/discovery is warned about and
skipped rather than preventing the rest of the harness from starting.

Imported tools are exposed both as ordinary model tools and through lazy `tool` builders inside
`script`, using the registered tool name shown in the catalog. Native MCP argument schemas are
preserved when they can safely coexist with Skyhook's common tool arguments. If a schema is open
or conflicts with the common `bg` argument, its MCP input is nested under an `arguments` object
instead. Schemas with a root `$ref` or `patternProperties` are also conservatively wrapped. Input
schema roots must declare `type = "object"`; tools with invalid or unsupported schemas are warned
about and skipped individually. Use the advertised schema for either surface; the wrapper is
removed before sending input to the MCP server. Tools normally use readable names such as
`mcp_filesystem_write_file`. A short, deterministic hash suffix is added only when a name needs
sanitizing or shortening to the 64-character limit, or conflicts with another tool. Ambiguous
names are resolved across the startup catalog independently of discovery order. Use the exact
advertised name with `tool[name](...)` or its fluent builder. Direct calls return normal job views,
while foreground script calls return the MCP result envelope (`content`, plus optional `structuredContent` and `isError`). Images use the
usual saved-output image handling, and server-reported errors retain their output in failed jobs.

Calls pass through the same cancellation and background-job machinery as builtins. Skyhook
bounds discovery and response sizes, and does not replay a call with an uncertain outcome after
a connection failure. Cancellation cannot undo effects that already occurred on the server.

### Model connection recovery

If a Codex WebSocket connection is interrupted before its response is committed, Skyhook
reconnects and requests a fresh response using the same committed history. Recovery stays within
the existing root turn or child job: completed tools are not rerun, and tool calls from an
interrupted response are never executed. Partial streamed output is marked as interrupted and
is not added to the model's history. Inputs received during recovery wait until the next normal
request boundary.

Connection recovery allows three attempts in total, with cancellation-aware delays of roughly
500 ms and 1.5 s plus up to 100 ms jitter. The UI and session journal show scheduled reconnects;
a child is marked failed only after recovery is exhausted. Context-window compaction has a
separate, additive budget, with at most five attempts at a logical agent response when both
recovery paths are needed (excluding compaction's separate summary requests). These limits
reset after a successful model response.

Recovery applies to eligible Codex WebSocket close/EOF, I/O, write, ping, and read-timeout
failures—not authentication failures, invalid requests, malformed protocol responses, explicit
provider aborts, or user cancellation. Native HTTP startup retry and Codex handshake fallback
policies are unchanged. Reconnecting clears the failed connection and its continuation state,
so the next attempt sends full history rather than continuing the interrupted response.

This prevents duplicate **Skyhook tool execution**, not duplicate provider inference: the
provider may have processed the interrupted request, and additional usage may be incurred.
Usage reported before interruption is retained; unreported provider usage remains unknown.
Recovery is not a general replay guarantee for provider-hosted side-effecting tools.

### Reasoning history and local-server compatibility

All backends retain returned reasoning and replay state in the session journal. Responses and Codex
replay native reasoning items (including encrypted state), and Anthropic replays signed thinking or
redacted-thinking blocks. Native replay is automatic when provider, endpoint, protocol, and model
provenance match. Visible summaries are not substituted for opaque or signed state. Switching to an
incompatible provider/model filters replay from that request without deleting the original history.
Reasoning that the service never returns cannot be reconstructed.

Standard Chat Completions has no portable request-side reasoning field. Compatible Chat providers
replay their returned, scoped reasoning using **`reasoning_content` by default**: this is consumed by
llama.cpp and SGLang, and accepted as an alias by current vLLM. Ordinary Chat responses without
reasoning do not acquire an invented reasoning field. Configure a different spelling or disable
request replay on the **provider**, not individual model profiles:

```toml
[providers.local]
kind = "openai"
base_url = "http://127.0.0.1:11434/v1"
api = "chat_completions"
# Optional; this is the default:
chat_reasoning_replay = "reasoning_content"
# Alternatives: "reasoning", or "unsupported" to keep reasoning locally only.
```

This option belongs only to OpenAI-compatible Chat Completions. Anthropic and Codex reject it as an
unknown setting; configuring it with `api = "responses"` is also rejected. Their native reasoning
replay remains automatic and independent of this option. All models using a provider share its
Chat wire convention; use separate provider entries if a proxy routes to incompatible conventions.
The agent runtime and generic `ModelRequest` never interpret this setting.

Reasoning is replayed with its owning assistant turn, including tool calls and reasoning-only turns;
it is never merged into answer text or fabricated `<think>` tags. Old Chat transcripts that contain
only display text without scoped replay provenance cannot safely be upgraded to native replay.
Compaction retains complete selected assistant/tool exchanges; older exchanges may be summarized to
keep active context bounded, while original journal events remain available.

OpenAI-compatible and Anthropic providers also accept positive `startup_timeout_secs` and
`read_idle_timeout_secs` settings (both default to 600 seconds). Startup is a deadline for each HTTP
attempt, while read-idle resets after each response-body chunk. For example:

```toml
[providers.local]
kind = "openai"
base_url = "http://127.0.0.1:8080/v1"
api = "chat_completions"
startup_timeout_secs = 600
read_idle_timeout_secs = 600
```

The Chat codec accepts llama-swap loading chunks with an absent singleton choice index, repeated
same-tool updates within one chunk (such as vLLM Hermes), and late cache-usage attribution. It still
rejects malformed/nonzero choice indexes, multiple answer choices, and unsupported semantic
extensions. Server-side tool and reasoning parsers/templates must be configured appropriately;
Skyhook does not infer them from model names. The current `max_completion_tokens` and usage-stream
fields are shared by OpenAI, llama.cpp, vLLM, and SGLang; no automatic parameter renaming is applied.

Native HTTP transport owns startup retries, with **at most three HTTP attempts** for pre-response connection/send
failures, startup/header timeouts, and transient HTTP 408/429/500/502/503/504 responses. Each attempt receives a fresh startup
deadline; three startup timeouts can therefore take about 30 minutes at the defaults, plus bounded
backoff. Exhausted transient failures include the HTTP attempt count. Short `Retry-After` delays are honored;
long or unparseable delays are returned to the caller rather than retried early. Cancellation drops
the pending request or retry wait. The server may continue work if it does not honor disconnects.

After successful HTTP headers are accepted, malformed SSE streams, read-idle timeouts, and partial
output are not replayed. Deterministic protocol/configuration failures and refusals are not regenerated
by the agent loop. Context-overflow recovery remains a separate compaction path. Codex retains
single-attempt HTTP; eligible WebSocket interruptions use the bounded runtime recovery described
above. Aborted or truncated tool generation never makes incomplete arguments executable.

The `codex` provider uses **Skyhook-owned** OAuth credentials. Run `skyhook auth login` for browser
authorization or `skyhook auth login --headless` for device authorization. `skyhook auth status`
reports local login status and `skyhook auth logout` removes Skyhook's credentials. Login does not
require a model configuration. Skyhook never imports, reads, or modifies the official Codex client's
credential files. Secure Codex credential storage currently requires Unix; other platforms fail
explicitly rather than writing tokens without private ACL guarantees. Claude subscription support
has been removed.

Migration from Flux is a breaking configuration change: replace `openai_compatible` with `openai`,
add explicit API-root URLs and OpenAI API choices, replace model aliases with full identifiers, and
log in separately for Codex.

Instructions in `AGENTS.md` files are loaded from outermost ancestor to workspace, followed by
instructions configured through the library. Root and child agents use the built-in system prompts;
there are no named agent profiles or `.agents/agents` role discovery.

## Embedded JavaScript

Every `script` call gets a fresh QuickJS runtime. Tool calls are lazy. Each builder
instance memoizes its own execution, so reusing one builder executes it once while constructing an
equivalent new builder creates a new call:

```js
const packageFile = tool.read({ path: "Cargo.toml" });
const matches = tool.search({ pattern: "TODO", path: "src" });

// The same schema also generates an immutable fluent builder.
const readme = tool.read().path("README.md");

// A skill loads its complete SKILL.md and a recursive asset tree.
const instructions = tool.skill({ name: "release" });
const template = tool.skill({ name: "release", path: "references/template.md" });

// Builders nested in the returned value are resolved concurrently.
return { packageFile, matches, readme, instructions, template };
```

The runtime also exposes:

- `Date`, `RegExp`, `Map`/`Set`, `Proxy`/`Reflect`, and `BigInt`;
- `ArrayBuffer`, `DataView`, and typed arrays, including `Uint8Array.fromBase64`,
  `.fromHex`, `.toBase64()`, and `.toHex()`;
- `performance.now()` for measuring elapsed milliseconds;
- `await sleep(ms)` for asynchronous waits, resolving to `undefined`. The delay must be a finite,
  nonnegative number of milliseconds within the host timer range; fractional values are accepted.
  Sleeps stop when the script is cancelled, and unawaited sleeps do not keep it alive;
- `new WorkPool(concurrency).map(items, worker)` and `.run([fn1, fn2, ...])` as async iterables
  yielding successful `{index, value}` results in completion order. `run` requires exactly one
  array of functions, each called with no arguments—not variadic arguments. Invalid `run`
  arguments throw before any task starts. Failures thrown by valid tasks are logged and skipped;
  remaining items continue. Early iterator closure stops scheduling and drains running work;
- `await receive()` waits for the next JSON value sent to the script's own job ID with
  `tool.job(scriptJobId).send({value})`; the script must be launched with `bg: true`.
  This is script input, not child-agent input: agents receive owner updates automatically.

For example, pass the task functions to `run` in one array:

```js
const results = [];
for await (const {index, value} of new WorkPool(2).run([
  async () => 1,
  async () => 2,
  async () => 3,
])) {
  results.push({index, value});
}
return results;
```

Read or search saved command output with `tool.job(commandJobId).output({field:"/result/stdout"})`.
Field-selected output is a job view, not a raw string: available text is in `preview.lines`,
with pagination metadata alongside it. Field, pagination, and search selections omit image
attachments; whole-output reads can attach saved images.

Before returning results, convert `BigInt` values to strings, dates with `.toISOString()`, and
typed arrays with `Array.from(bytes)`, `.toBase64()`, or `.toHex()`. The runtime does not provide
Node.js APIs, `fetch`, `URL`, `TextEncoder`/`TextDecoder`, or `setTimeout`/`setInterval`.

Every script result is `{value: <JavaScript return>, console: <captured text>}`, including
silent scripts (`console: ""`) and scripts without a return (`value: null`). This wrapper applies
to both public job views and native/programmatic script results; only script results need this
extra `.value` unwrapping. Ordinary tool results inside JavaScript remain unchanged. In a script
job view, the return is at `/result/value` and logs are at `/result/console`.

`console.log(...values)` captures space-separated text, formatting objects as JSON. Logs are
delivered at completion, not streamed, and are capped at 16 MiB per script with an explicit
truncation marker. On failure, the result is `{value: null, console: <captured text>, failure:
<details>}` alongside the job error. Console text belongs to the script result, not generic job
metadata or a separate tool-result text block.

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
  Custom providers implement both traits. A request's correlation must match its context's identity.
  The common `ResponseEvent` contract separates output items from independently streamed blocks:
  `ItemStarted`, `BlockStarted`, typed `BlockDelta`, `BlockEnded`, `ItemEnded`, `UsageUpdated`, and
  `ResponseEnded`. Item/block IDs identify content; explicit positions determine its order.
  Start events declare kinds, block ends contain authoritative final content, and item ends attach
  replay metadata once. Each reasoning summary part is a separate visible block with its own end;
  encrypted-only reasoning items need not create an empty visible section.
  `ResponseAssembler` validates lifecycles and supplies ordered snapshots to both the runtime and
  observation/UI layers. Cumulative usage snapshots replace earlier values, and response termination
  carries a stop reason rather than a truncation boolean. Legacy unindexed delta events are removed.
  Completed assistant messages persist nested items/blocks with their IDs and positions. Session
  journals now use format version 2; version-1 journals are rejected with an explicit unsupported-version
  error rather than guessing the missing block boundaries. Start a new session after this upgrade.
  Reasoning
  replay payloads retain provider/endpoint/protocol/model provenance: incompatible private reasoning
  is omitted from outgoing requests, not deleted from the stored transcript.
  Input images are encoded or rejected explicitly, never silently dropped. Protocols without
  image-bearing tool results adapt images into adjacent user content associated with the tool call.
  Chat Completions also accepts the common `reasoning_content` / `reasoning` streaming fields used
  by local Qwen servers, preserving separate visible reasoning blocks and scoped replay envelopes.
  Chat request-side replay uses the provider's `chat_reasoning_replay` field selector (default
  `reasoning_content`);
  other APIs replay their compatible native reasoning state automatically. Null optional extension
  fields are tolerated; unsupported
  nonempty semantic fields fail explicitly. Generated image outputs are not supported.
  Chat streaming compatibility is provider-neutral: absent/null choices and deltas are normalized
  to empty containers, and metadata-only chunks are harmless before `[DONE]`. After `finish_reason`,
  usage and no-op deltas (empty/null text, empty tool-call lists, assistant role headers, and null
  extensions) remain accepted, with or without usage; identical finish reasons are idempotent.
  Extra envelope, choice, and usage metadata is ignored. Actual post-finish output, conflicting
  finish reasons, unsupported non-null delta fields, malformed tool calls or usage, and data after
  `[DONE]` still fail. Metadata never substitutes for a finish reason at EOF or `[DONE]`.
- Each agent loop owns an `AgentContext`: projected journal history, model profile and request
  template, token accounting, and its provider handle. Codex contexts have separate WebSocket
  connection and continuation state, with shared authentication tokens. Connection setup failures
  may fall back to HTTP. The provider never internally replays a submitted WebSocket request;
  the runtime can reset the connection and retry an uncommitted response within its recovery budget.
  Handles survive turns, retries, questions, and compaction, and are released when the agent exits.
  Model-profile changes prepare a replacement before committing, preserve history, and reset token
  calibration. Equivalent profiles retain their handle. Resuming opens a fresh handle with the
  same agent cache identity; compaction clears calibration from the previous history projection.
- `tool::ToolRegistryBuilder` supports typed and JSON-based tools, while `tool::executor::ToolExecutor`
  turns every invocation into a supervised job. Typed registrations generate both input and output
  schemas; compact result shapes are included in model and script documentation.
- `session::SessionStore` persists a versioned append-only JSONL log, content-addressed image blobs, job
  outputs, and line-addressable job output.
- `agent::Harness` owns model profiles and policy; each `agent::SessionHandle` owns an isolated agent tree
  and registry.
- Child agents retain their conversation for follow-up work.
  `tool.job(id).send({value: instructions})` delivers unsolicited input automatically at a running
  child's next model-request boundary, without interrupting the current request or tools.
  Children do not need `receive()` to read these updates. Every visible child text reply,
  including text-only and final replies, is delivered independently through the background-job
  event path and wakes `wait`, without waiting for the child job to finish. Message events carry
  `kind: "message"`, the child job `id`, source `message` sequence, optional `name`, and `text`.
  Completion is determined separately by the agent's remaining work and queued inputs, not by
  message delivery. A completed child notification references its `last_message` instead of
  repeating that reply, as do automatic model tool responses for completed children. Explicit
  `job_output` reads and native script/host calls still expose the saved final result.
  Progress text is not concatenated into that final result. A foreground call still needs to
  return before its parent can make another model request.
  Messages and terminal/question notifications share a snapshot → parent-history commit →
  acknowledgment boundary. Failed or abandoned preparation leaves notifications pending, and
  caller cancellation cannot split a started append/acknowledgment operation. Pending messages
  are recovered from committed child history after restart; committed parent notifications
  acknowledge each message independently and prevent duplicate delivery. Reading or claiming a
  job result does not consume its message events. Legacy message notifications and exact final
  replies in completed runtime notifications are recognized on replay. An old output-claim marker
  alone is not evidence of message delivery, so such a reply may be delivered again rather than
  discarded. Delivery means inclusion in parent history at a request boundary, not interruption
  of an in-flight model request.
  Sending to a completed child appends
  the instructions after its existing
  conversation and starts a new request under the same agent and job ID; it does not start
  over with fresh history. `job_output(id)` then exposes the latest run's saved result, not an archive
  of earlier results; already-committed parent notifications and the child conversation remain intact.
  This resumption applies to successfully completed agent jobs retained
  in the live runtime, not arbitrary completed tools or jobs restored after a process restart.
  Failed, cancelled, and interrupted agents are not resumed by `send`.
  Each model-facing `ask` contains one
  `{id, prompt, options}` question; independent concurrent calls are merged by the runtime. A child
  question batch changes its stable agent job to `waiting_input`; answer that job with
  `tool.job(id).send({value: answer})` in a script. For a merged batch, use
  `tool.job(id).send({value: {question_id: answer, another_id: answer}})`; the keys are the IDs
  included in the waiting job's `questions` output. Each answer can be a string (a suggestion
  label or free-form text), or `{"answer": "selected label", "comment": "user text"}` for a
  suggestion with a non-whitespace comment. A single question returns that value directly;
  a merged batch keeps each value under its question ID.
  Root-agent questions still go directly to the host question handler.
  `ask` also accepts optional `bg` (boolean, default `false`). With `bg: true`, the call returns
  job metadata immediately so the agent can continue independent work. Use `job_output` or
  `tool.job(id).output()` to inspect the pending question and eventual answer; the ask job also
  accepts `tool.job(id).send({value: answer})`. Omitting `bg` or passing `false` keeps the usual
  foreground wait. Multiple outstanding child question batches are combined on the stable agent
  job; parents may answer a subset keyed by question ID, and unanswered questions remain pending.
  IDs must be unique across that child's outstanding questions. Background questions are still
  owned jobs: children must await or cancel them before finishing.

`agent` accepts an optional `depth` delegation budget. It defaults to zero, making the launched
child a leaf. A caller may grant less than its own available depth; once no depth remains, `agent`
is omitted from both model tools and script bindings. Its optional `workspace` accepts relative or
absolute directories for both local and remote children. Relative overrides resolve against the
workspace selected by the target rules. Children receive a fresh conversation, shared harness
instructions and host-owned skills, and their parent's active model, including model switches and
restored session selections. An explicit child `model` overrides the inherited model.
This applies equally to local children, remote children, and deeper descendants. Agents on
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

`agent`, `exec`, `shell`, and `fetch` accept an optional `name` describing the work. Names must use
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
required; seed items use the same `{text, status}` format as `todo.items` and appear in the
child's runtime state before its first model request:

```js
const child = await tool.agent({
  prompt: "Implement the requested change and report the validation results.",
  name: "implement-change",
  todos: [
    {text: "Inspect the implementation", status: "in_progress"},
    {text: "Make the change", status: "pending"},
    {text: "Run relevant checks", status: "pending"}
  ],
  bg: true
});
// Yield for an event, then inspect the child; an event need not mean completion.
await tool.wait({timeout:300});
const childStatus = await tool.job(child.id).output();
if (childStatus.state === "queued") return childStatus;
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

Each model request ends with one fresh, compact-text `<skyhook_state>` snapshot. Its first
line is `date:YYYY-MM-DD`, using the host's current local date, refreshed per request rather
than fixed in the system prompt at agent startup. The optional `jobs:` and `todos:` sections
follow in that order; empty sections are omitted. The system prompt explains this format once.
This presentation does not change the JSON returned by job or todo tools.

The jobs section starts with `jobs: job parent tool name state age_s turns tool_calls`.
Each following row contains those fields separated by single spaces. `parent` is `-` for
jobs directly visible to the caller; nested active child-agent rows identify their immediate
parent job explicitly. Ordinary tool jobs are not included as nested children. Tool and name
values are unquoted only when they consist of ASCII letters, digits, `_`, `-`, `.`, or `/`;
other values, including a literal `-`, are JSON-quoted. Missing names or counters use `-`.

`age_s` counts elapsed whole seconds since job creation, including time queued or waiting for
input, clamped to zero if the clock moves before the creation timestamp. Child-agent jobs report
the child's selected target and workspace once initialized. Optional `target="..."` and
`workspace="..."` fields use JSON-quoted values and appear only when they differ from the
snapshot's current execution location. Every row is compared with that location, never with
its parent row; omitted fields do not inherit a parent's overrides. Target fields are omitted
when target capabilities are disabled.

Active agent jobs include exclusive `turns` and `tool_calls` counters. A turn is a complete,
committed assistant response (including responses containing tool calls); failed attempts,
incomplete responses, retries, and compaction requests do not add turns. Tool calls count jobs
launched by that child, including calls inside its scripts, but not work owned by its descendants.
Counters remain cumulative across retained-child follow-ups and session resume. Counters are
per-agent, not subtree totals.

The todos section retains every item, including completed items, in its original order. Each
consecutive run of the same status starts with `pending:`, `in_progress:`, or `completed:`;
each item's text follows on its own line, indented by two spaces and JSON-quoted. A status
heading repeats if that status occurs again after another status; items are not globally regrouped.

```text
<skyhook_state>
date:2026-09-05
jobs: job parent tool name state age_s turns tool_calls
7 - exec run-tests running 12 - -
8 - agent review running 9 2 3 workspace="/home/user/review"
9 8 agent - waiting_input 4 1 0
todos:
completed:
  "Inspect the implementation"
in_progress:
  "Make the change"
pending:
  "Run relevant checks"
</skyhook_state>
```

In this example, job 9 uses the snapshot's current location, not job 8's workspace override.
With no active jobs or todos, only the date line remains inside the state block.

These snapshots are assembled at request time
and never appended to durable conversation history. Actual job notifications, tool exchanges,
and todo replacement events remain durable. Provider caching behavior is
unchanged; transient history does not guarantee exclusion from provider KV caches.

## Built-in tools

`read`, `search`, `glob`, `exec`, `shell`, `fetch`, `write`, `replace`, `patch`, `remove`, `script`, `targets`,
`target_add`, `jobs`, `job_output`, `wait`, `ask`, `todo`, and `agent`. `jobs()` lists the current agent's
active jobs, excluding the listing call and its containing script. `jobs({all:true})` includes
completed history; listings contain status and references, never saved results.

`job_output` reads saved output and status immediately; it never waits for new output or completion.
Scripts use `tool.job(id).output(...)`, `.send({value})`, and `.cancel()`.

Use `wait({timeout?: seconds})` (or `await tool.wait(...)` in scripts) to yield until an agent
event or a timeout, then inspect the relevant jobs. Omitted or null `timeout` waits indefinitely;
a supplied timeout must be a positive integer number of seconds.
The result is `{reason:"event"}` or `{reason:"timeout"}`. An event does not guarantee a particular
job has completed; inspect its current status. Waiting does not stop background work.
Do independent work first rather than polling output in a tight loop. Output reads reject the old
`wait` argument.

### Creating files with `write`

`write({path, content})` atomically creates or replaces a UTF-8 file. Set the optional
`create_parents: true` to create missing parent directories recursively before writing, for example
`write({path: "reports/run/summary.md", content: "...", create_parents: true})`.
It defaults to `false`, so existing calls still fail when a parent directory is missing.

### HTTP requests with `fetch`

`fetch` is a reqwest-backed HTTP tool, available directly and as `tool.fetch(...)` in scripts.
It runs on the selected execution target: DNS, TLS, proxy discovery, uploads, and downloads all
happen there. Relative paths use that target's workspace; no files are implicitly copied between
machines. Like `exec`, it supports `name`, `bg`, cancellation, and saved job output.

```js
// Read an article without sending HTML boilerplate to the model.
const page = await tool.fetch({url: "https://example.com/article", text: true});

// Send JSON to an API. HTTP 4xx/5xx are responses, not tool failures.
const response = await tool.fetch({
  url: "https://api.example.com/items",
  method: "POST",
  body: {kind: "json", value: {name: "example"}}
});

// Duplicate query parameters and request headers are supported.
const search = await tool.fetch({
  url: "https://api.example.com/search",
  query: [["tag", "rust"], ["tag", "http"]],
  headers: {Accept: "application/json"}
});

// Send a file as the raw request body, without base64-encoding it in model context.
const upload = await tool.fetch({
  url: "https://api.example.com/upload",
  method: "PUT",
  headers: {"Content-Type": "application/gzip"},
  body: {kind: "file", path: "dist/archive.tar.gz"}
});

// Save a download on the execution target. Existing files are preserved unless overwrite:true.
const download = await tool.fetch({
  url: "https://example.com/archive.tar.gz",
  save_to: "archive.tar.gz",
  max_bytes: 104857600,
  timeout: 300
});

// Explicitly opt out of certificate validation for a development HTTPS server.
// This permits untrusted certificates and enables interception; never use casually.
const development = await tool.fetch({url: "https://localhost:8443/health", insecure: true});
```

Only `url` is required. `method` defaults to `GET` and accepts standard methods and valid custom
HTTP method tokens. `query` is an ordered array of string pairs, appended to any existing URL query.
The default `User-Agent` is `Skyhook/<version>`; an explicit `User-Agent` header overrides it.
Header values can be strings or arrays of strings. `auth` accepts `{kind:"bearer",token:"..."}` or
`{kind:"basic",username:"...",password:"..."}`; arbitrary authentication schemes can use headers.
Do not combine `auth` with an `Authorization` header or embed credentials in URLs.

`body` has exactly one tagged source:

- `{kind:"text",value:"..."}` for UTF-8 text.
- `{kind:"json",value:...}` for any JSON value, including `null`.
- `{kind:"form",fields:[["key","value"],...]}` for URL-encoded forms with repeated keys.
- `{kind:"base64",value:"..."}` for inline binary data.
- `{kind:"file",path:"..."}` for a regular-file upload as the raw request body.

Multipart form-data encoding is not supported. File bodies can set their media type through the
`Content-Type` request header; they are not interchangeable with multipart uploads.

Responses include final URL/method, status, `ok` (2xx), redirect history, received byte count,
elapsed time, and a tagged `body`: `text`, `base64`, `file`, or `empty`. Response headers are omitted
by default; set the optional `include_headers: true` to return them as a map of repeated values.
`include_headers` defaults to `false` and does not affect the `headers` request-header map.
JSON responses remain decoded text; scripts can use `JSON.parse(response.body.text)`.
`response_format` defaults to `auto` (text for textual content, base64 otherwise); `text` forces
character decoding and `base64` preserves response entity bytes. HTTP decompression is automatic;
these are not raw wire bytes. `save_to` streams to a temporary file and commits on success instead
of embedding the payload. Errors or cancellation do not replace an existing destination.
Automatic job presentation may shorten `body.text` and `body.data`, with continuation markers;
retrieve the complete saved payload using `job_output` fields `/result/body/text` or
`/result/body/data`. Status, opted-in headers, body kind, and other metadata remain intact. JavaScript
calls still receive the complete payload for processing.

`text:true` is separate from `response_format:"text"`: it extracts readable article content from
HTML with **dom_smoothie**, returning plain text and available title/byline/site/language metadata.
It does not execute JavaScript or fetch linked assets. Plain text, JSON, and other textual types
pass through decoded; binary content is not converted. Extraction failures are explicit, never
silently replaced with raw HTML. Fetch with `text:false` to inspect the original response. Empty
responses remain empty. `text:true` cannot be combined with `save_to` or `response_format:"base64"`.
Extraction accepts at most 10 MiB of decoded HTML and 50,000 DOM elements, with bounded parser
concurrency. Character decoding honors BOMs, HTTP charsets, and HTML meta charsets where applicable.

The default total `timeout` is 30 seconds, `connect_timeout` is 10 seconds, and `max_bytes` is
10 MiB. The response limit can be raised to 100 MiB; total uploads are also capped at 100 MiB.
Timeouts must be between 1 and 3600 seconds and `max_redirects` cannot exceed 20.
Limits apply while streaming, including to decompressed data; exceeding a limit fails
rather than reporting an incomplete body as successful. Model-visible preview truncation is
independent: retrieve saved results with `job_output`. Cancellation and timeouts cannot undo
server-side effects, and requests are not automatically retried.

Transport and processing failures remain failed jobs (and rejected script calls), but include a
structured failure result. It contains `method`, a safe `origin`, `elapsed_ms`, `received_bytes`,
redirect history, and `diagnostic`: `phase`, `error_kind`, and a concise `message`. To keep tool
definitions compact, diagnostic category fields use string schemas rather than exhaustive lists
of labels; the typed runtime classifications and returned values are unchanged. When available,
`diagnostic.os_error` supplies the executing platform, a numeric OS `code`, and a portable `kind`.
Timeouts include `diagnostic.timeout.kind` (`total`, `connect`, or `unknown`) and a `limit_ms` only
when the expiring limit is known. Timing starts inside fetch on the execution target; it does not
include SSH startup or initial tool approval.

Connection refusal, host/network unreachability, DNS failures, TLS failures, typed HTTP proxy
CONNECT failures, and response-processing failures are distinguished when the underlying errors
provide evidence. Otherwise fetch reports a generic transport category; it never infers that a
firewall caused an error. Diagnostic messages do not copy arbitrary error strings, query strings,
credentials, headers, or bodies. Failure URL/redirect context is reduced to origins. Received HTTP
headers are included only with `include_headers: true`; they retain their normal response semantics
and may still contain sensitive response data.
`proxy_origin`, when present, describes an explicit proxy; omission does not rule out an environment
proxy. Use the job's target for source attribution, and interpret OS codes using the reported platform.

If headers arrived before a failure (including an outer timeout), the failure also retains the
HTTP status, `ok`, and byte count, plus headers when `include_headers: true`. Before any response,
those HTTP fields are omitted,
not fabricated. HTTP 4xx/5xx responses still complete normally: a 405 establishes HTTP connectivity,
not successful ingestion. Permission denial and cancellation retain their separate semantics.

In scripts, catch failures inside each worker and inspect `error.output.diagnostic`; merely
logging an Error or allowing `WorkPool` to skip a failed worker loses the structured row:

```js
try {
  const response = await tool.fetch({url, target});
  return {target, http_reached: true, status: response.status};
} catch (error) {
  const failure = error.output ?? {};
  return {
    target,
    http_reached: Number.isInteger(failure.status),
    status: failure.status ?? null,
    elapsed_ms: failure.elapsed_ms ?? null,
    diagnostic: failure.diagnostic ?? null,
    error: error.message,
  };
}
```

The same diagnostic is retrievable from a failed fetch job at `/result/diagnostic`. An uncaught
script failure preserves it under `/result/failure/output/diagnostic` in the script job.

`redirects` is `safe` by default (follow GET/HEAD), `follow` to follow other methods too, or `manual`
to return the redirect response. `max_redirects` defaults to 5. Changed origins require authorization;
cross-origin requests do not inherit sensitive request headers, and HTTPS-to-HTTP redirects are
rejected. Redirect method rewriting follows HTTP conventions; 307/308 preserve method and body.
`proxy` selects an explicit HTTP proxy; otherwise reqwest uses the target's proxy environment.
`insecure` defaults to **false**. Setting it to **true** disables HTTPS certificate validation for
that invocation only; it does not disable authorization or permit HTTPS downgrade redirects.

All requests require the `network` capability and approval, not just filesystem `read` permission.
File uploads additionally require `read`, and downloads require `write`, including paths inside
the workspace. Loopback and internal-service URLs are supported; this is a general-purpose network
tool, not an isolated browser or a network sandbox. Returned content is untrusted data.
There is no ambient shared cookie jar: set `Cookie` explicitly and use `include_headers: true`
to inspect repeated `Set-Cookie` headers when needed. **Arguments, response bodies, and headers can contain secrets and are subject
to the normal session/job persistence rules**; do not assume HTTP credentials are omitted from
session records or that `insecure` makes authentication safer.

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
Missing paths and operating-system access denials from `read` are successful tool results with
`{kind:"error", path, error:{code:"not_found"|"permission_denied", message}}`, so workflows can inspect the
error without catching an exception. Tool-policy permission denials remain tool errors.
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
`matches`, glob `paths`, skill asset `content` and `assets` trees, and script result `console` text. Skill instructions
remain complete. Script results retain `console: ""` even for silent scripts; ordinary tools have
no console field. Shortened fields keep their original types;
`truncated: [{field, total_lines, next_start, next_offset?}]` identifies each one, reports its
total source lines, and supplies the exact first unread position. Finished jobs with an incomplete capture include an `Output incomplete.` notice. Errors
and questions are returned in full. Schemas are persisted with jobs so these rules also apply
after session resume and to completed remote jobs.

Full JavaScript tool results remain available for programmatic transformations. When a script
returns an unchanged tool-result object or array, it is presented as that child's native job view,
wherever it appears within the script result's `value` structure. The view replaces the raw tool result and carries
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
job_output({job:42, field:"/result/stdout", start:22, offset:54, limit:100})
```

`field` is a JSON Pointer: `/result/content` selects a file snapshot, `/result/stdout` and
`/result/stderr` select process streams. For scripts, `/result/console` selects captured console
text and `/result/value` selects the JavaScript return (append pointer segments for nested data,
for example `/result/value/items`). Objects and
arrays have deterministic JSON text views. Long lines are split into UTF-8-safe fragments;
the next position identifies where to continue. Regex matching is case-sensitive unless inline
flags override it. Matching supports lines up to 4 MiB and reports an explicit resource error
for larger lines; ordinary paging can still read those lines.

`start` is one-based (default 1); `offset` is a zero-based UTF-8 byte offset within that
starting line (default 0). `limit` is 1–1000 returned source lines (default 100), including
match context. `context` is 0–20 surrounding lines (default 0); positive context requires `pattern`.
Every argument with a default is optional in the tool schema. Output inspection has no wait argument.

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

Host-owned skills are exposed through `skills` and `skill`. Discovery reads `~/.agents/skills`
and `.agents/skills` directories along the workspace ancestry, with the nearest workspace
definition winning when names collide. Discovery happens when the harness starts; restart it
to discover newly added skills. A skill directory contains `SKILL.md` and optional supporting
files, conventionally grouped in `scripts/`, `references/`, and `assets/`:

```text
.agents/skills/release/
├── SKILL.md
├── assets/
│   └── logo.png
├── references/
│   └── template.md
└── scripts/
    └── release.py
```

Only `name` is required by the `skill` input schema. `path` and `to` are optional and nullable;
native calls supplying `null` behave like calls omitting those fields.

```js
await tool.skill({ name: "release" }); // Complete SKILL.md plus recursive assets tree
await tool.skill({ name: "release", path: "references" }); // Directory subtree
await tool.skill({ name: "release", path: "references/template.md" }); // Original text
await tool.skill({ name: "release", path: "assets/logo.png" }); // Attached image
await tool.skill({ name: "release", path: "assets/logo.png", to: "tmp/logo.png" }); // Copy
```

Results have a `kind` discriminator: `skill`, `directory`, `text`, `image`, `binary`, or `copied`.
Base skill results contain `name`, `description`, complete `content`, and an `assets` text tree
that includes nested files but excludes the already-loaded root `SKILL.md`. Directory results
use the same tree format, rooted at the selected path. Trees mark symlinks without descending
through them. Long trees and asset text are saved in full and can be paged using `job_output`.

Text assets (including JSON, YAML, source code, and SVG) are returned unchanged, never executed.
Supported raster images are attached with metadata. Other binary files return metadata and
a suggestion to supply `to`, rather than raw binary or base64 content. Copies return destination,
byte count, and SHA-256, and require write authorization. `to` is resolved in the **calling agent's\ntarget and workspace**: a remote agent receives host-owned asset bytes on its remote target,\nnot in a similarly named directory on the host. Skill discovery, instructions, and asset reads\nremain host-owned. Remote copies require route and destination write authorization, including\npath approval when the destination is outside the authorized workspace. `to` requires a file\n`path`; paths inside a skill must be relative and cannot escape its root. Use `path: "."` to\nbrowse the root.

A mixed-file test workspace lives at `tests/fixtures/skill-workspace/`, including its hidden
`.agents/skills/mixed-assets/` directory. Its tests exercise native calls, explicit nulls,
asset discovery, UTF-8 and binary files, image attachments, copying, and path containment.

## Development

```sh
cargo test -p skyhook-agent-core --all-targets
cargo test -p skyhook-agent --all-targets --no-default-features --features tui
cargo clippy --workspace --all-targets --no-default-features --features tui -- -D warnings
```

### Remote transport architecture

Remote connection protocols use the production `ConnectionFactory` interface in
`crates/skyhook-core/src/remote/backend.rs`. Factories return an owned `Transport` byte stream;
the manager performs the common shim handshake and client setup for both real backends and test
doubles. Protocol selection and origin-side connection operations go through the backend dispatch
layer, rather than constructing SSH launchers in the manager or target router.

- `remote/manager.rs` owns route/origin handling, workspace connection pooling, cancellation, and
  invalidation.
- `remote/client.rs` owns shared shim RPC and relayed stream state;
  `remote/transport.rs` defines the owned asynchronous byte-stream contract.
- `remote/backends/ssh/` contains OpenSSH configuration/bootstrap, process supervision, askpass,
  and session-owned authentication. The public `remote::ssh` path remains a compatibility export.
- `remote/protocol.rs` is the **shim wire protocol**, not the SSH/WinRM transport interface.
  Existing SSH control messages remain typed compatibility adapters; worker services dispatch
  those operations through the backend layer.

A backend must preserve credential ownership on `origin` (not `via`), retain its origin connection
for the lifetime of child streams, and release its resources during session shutdown. Cancelling
one waiter must not terminate a connection startup shared with other callers. Adding another
protocol still requires its own target configuration, resolution, authentication, and bootstrap;
the abstraction does not make SSH shell commands or ProxyJump semantics universal.

### Embedded shim builds

Build both statically linked Linux SSH shims and the release CLI with
`cargo build --release -p skyhook-agent`. This default-feature build requires Zig and both Rust
musl targets listed above; it does not require a container engine. Zig 0.16.0 was used to validate
both targets.

Debug builds (`cargo build`, `cargo test`, and `cargo install --debug`) build the shims with
Cargo's `dev` profile: no optimization or stripping by default, with debug information and
debug assertions. Release builds keep the size-optimized, stripped `shim-release` profile.
Both modes use the same musl targets and static ELF validation, and publish to the same artifact
filenames in `target/shims/`. Switching build modes replaces those files with the matching variants;
debug artifacts and the binaries embedding them are substantially larger.

The build matrix is the `SHIM_TARGETS` constant in `build.rs`. Each entry declares `platform`,
`protocol`, `arch`, and the Rust `target` triple. The Cargo binary name is derived as
`<platform>-<protocol>`; the saved artifact is `<platform>-<protocol>-<arch>`. The runtime discovers
embedded filenames and selects by protocol, platform, and architecture after probing the remote
machine. The catalog also understands executable extensions, such as
`windows-winrm-x86_64.exe`, but no Windows build target or WinRM transport is implemented yet.

`rust-embed` embeds both the file list and bytes in debug and release builds, so installed binaries
do not need the build-time `target/shims/` directory. Local-only and rust-analyzer builds skip embedding even
if artifacts already exist. Shim builds deliberately ignore host Rust flags (including Cargo
configuration rustflags) to keep the artifacts portable.

The fixed staging location is `<package>/target/shims/`, even when `CARGO_TARGET_DIR` or
`--target-dir` redirects compilation elsewhere. It must be writable; intermediate shim builds
remain isolated under Cargo's `OUT_DIR`. The embedder allows a missing staging directory for
clean-tree tooling, but normal embedding builds must successfully generate and validate their
shims before the catalog is enabled.

Generated files are published atomically and unchanged bytes are not rewritten; unrelated
artifacts are preserved. Remove obsolete artifacts explicitly when changing the build matrix,
and avoid simultaneous builds writing to the same staging directory.

Cargo excludes the package's `target/` directory from source packages and package-verification
source comparisons. Staging there therefore avoids the source-modification failure of the old
source-root `shims/` location, without requiring local-only verification flags:

```sh
cargo build --release -p skyhook-agent --locked
cargo package -p skyhook-agent --locked
```

Skyhook is licensed under AGPL-3.0-only.

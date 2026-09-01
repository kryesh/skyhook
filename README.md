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
`-s/--script`. Without either option, the CLI reads prompts interactively. Its default policy allows workspace reads and agent
state operations, while workspace writes and process execution require confirmation.
Set top-level `approve_all = true` in the config for the same non-interactive approval behavior.
Streamed assistant messages and concise tool-start summaries are prefixed with their session-local
agent ID (`root`, `1`, `1:1`, and so on), so root and child-agent activity remains distinguishable
during concurrent workflows. When the CLI closes the session, it prints cumulative output, total
input, and uncached input token counts.

## SSH targets

Targets are named directly under `[targets.<name>]`. Importing concrete aliases from the user's
SSH configuration is disabled by default. Session tools can upsert targets without modifying TOML.

```toml
[targets]
import_ssh_config = false

[targets.bastion]
host = "bastion.example.com"
user = "gateway"

[targets.build]
host = "build.internal"
workspace = "/srv/project"
via = "bastion"

[targets.build.auth]
kind = "key"
path = "~/.ssh/build_ed25519"
```

Authentication kinds are `openssh`, `agent`, `key`, and `interactive`. Interactive secrets are
requested by the host with terminal echo disabled and never enter tool arguments or session logs.
`via` references another named target and may form an acyclic jump chain.

Use `target` with `exec`, `shell`, or `agent`; `root` explicitly selects the local invocation.
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
version = 1
default_model_profile = "local"
approve_all = false

[providers.local]
kind = "openai_compatible"
base_url = "http://127.0.0.1:11434"
api = "chat_completions"

[models.local]
provider = "local"
model = "qwen3-coder"
max_output_tokens = 8192
supports_images = false
```

The `codex` and `claude` provider kinds import and refresh credentials through Flux, including
credentials from the official Codex and Claude CLIs. Instructions in `AGENTS.md` files are loaded
from outermost ancestor to workspace, followed by instructions configured through the library.

## Embedded JavaScript

Every `script` call gets a fresh, memory-limited QuickJS runtime. Tool calls are lazy and memoized:

```js
const packageFile = tool.read({ path: "Cargo.toml" });
const matches = tool.search({ pattern: "TODO", path: "src" });

// The same schema also generates an immutable fluent builder.
const firstLines = tool.read().path("README.md").start(1).limit(40);

// Selecting a skill loads its SKILL.md; selecting an asset changes the operation.
const instructions = tool.skill("release");
const template = tool.skill("release").asset({ path: "template.md" });

// Builders nested in the returned value are resolved concurrently.
return { packageFile, matches, firstLines, instructions, template };
```

The runtime also exposes:

- `new WorkPool(concurrency, { failFast: true })` for bounded, ordered, fail-fast work; set
  `failFast: false` for per-item `{ok, value|error}` results;
- `new Queue()` as an async iterable queue;
- `receive()` for input sent to the owning job (background scripts only);
- `notify(value)` for durable job progress.

Builder setters and object arguments come from the same strict JSON schema; omitted values receive
the handler's normal defaults. Awaiting a builder executes it immediately. Returning builders recursively executes independent
branches concurrently. All executions still pass through the same registry, policy hook, job
supervisor, persistence, and path authorization checks as model-originated calls. Top-level
`undefined` returns JSON `null`; nested `undefined` values are rejected with their result path.

## Library architecture

- `provider::Provider` returns an object-safe, asynchronously pollable response handle; concrete
  adapters live under `provider::backends` and wire types under `provider::protocol`.
- `tool::ToolRegistryBuilder` supports typed and erased tools, while `tool::executor::ToolExecutor`
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
absolute directories for both local and remote children.

Filesystem tools likewise accept relative or absolute paths. Canonical paths outside the configured
workspace require approval, cached in memory by session, target, access mode, and path; directory
approval covers descendants. Read and write grants are separate. Process execution remains subject
to confirmation on each invocation.

Background-capable tools accept a common optional `bg` argument. Unclaimed completions and child
questions are retained, injected exactly once into their owning agent, and wake it for another turn.
On resume, unfinished jobs are marked
interrupted and job identifiers continue monotonically. Hosts can call `SessionHandle::interrupt` to
stop active provider streams and cancel jobs across the session's agent tree.

## Built-in tools

`read`, `search`, `glob`, `exec`, `shell`, `write`, `replace`, `patch`, `remove`, `script`, `targets`,
`target_add`, `jobs`, `wait`, `ask`, `todo`, and `agent`. `jobs()` returns the current agent's active
jobs and excludes the listing call itself; use `jobs({all:true})` to include terminal history. The
remaining job controls are kept out of model tool definitions and exposed to scripts as
`tool.job(id).inspect()`, `.send({value})`, `.cancel()`, and `.events({after, limit})`;
`.wait({timeout})` uses the same `wait` tool.
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

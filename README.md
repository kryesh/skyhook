# Skyhook

Skyhook is a provider-neutral coding-agent harness with a programmable JavaScript orchestration
runtime. A tool is registered once and is then available through the model tool protocol and as a
lazy builder inside `script`.

This repository currently provides the reusable Rust library and a thin interactive CLI. The core
does not depend on a provider-specific response type; Flux adapters supply OpenAI, Anthropic,
Codex/ChatGPT subscription, Claude subscription, and OpenAI-compatible backends.

## Run

Rust 1.88 or newer is required.

```sh
cp skyhook.example.toml ~/.config/skyhook/config.toml
export OPENAI_API_KEY=...
cargo run -- --prompt "inspect this repository"
```

Use `--config path.toml` to use an explicit config instead of the user config, `--workspace PATH`
to choose the tool root, and `--resume SESSION_ID` to reopen a durable session. Pass a one-shot
prompt with `-p/--prompt`, or run a JavaScript workflow file through the registered script tool with
`-s/--script`. Without either option, the CLI reads prompts interactively. Its default policy allows workspace reads and agent
state operations, while workspace writes and process execution require confirmation.

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
Remote agent state machines run in the shim while providers, approvals, and canonical session
logging remain host-owned.

At compile time `rust-embed` includes files under `dist/shims` named
`skyhook-shim-<arch>-<os>[.<extension>]`. Empty and partial artifact sets are valid; selecting a
platform without an embedded shim returns an unsupported-platform error.

## Configuration

Providers and models are separate named profiles. API secrets are read from environment variables;
they are not stored in the TOML file. `openai_compatible` accepts an optional `api_key_env`, so a
local endpoint can be keyless. Its `api` is either `chat_completions` or `responses`.

```toml
version = 1
default_model_profile = "local"

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

- `new WorkPool(concurrency, { failFast })` for bounded parallel work;
- `new Queue()` as an async iterable queue;
- `receive()` for input sent to the owning job (background scripts only);
- `notify(value)` for durable job progress.

Builder setters and object arguments come from the same strict JSON schema; omitted values receive
the handler's normal defaults. Awaiting a builder executes it immediately. Returning builders recursively executes independent
branches concurrently. All executions still pass through the same registry, policy hook, job
supervisor, persistence, and workspace containment checks as model-originated calls.

## Library architecture

- `provider::Provider` returns an object-safe, asynchronously pollable response handle; concrete
  adapters live under `provider::backends` and wire types under `provider::protocol`.
- `tool::ToolRegistryBuilder` supports typed and erased tools, while `tool::executor::ToolExecutor`
  turns every invocation into a supervised job.
- `session::SessionStore` persists a versioned append-only JSONL log, content-addressed image blobs, job
  outputs, and cursor-addressable job progress.
- `agent::Harness` owns profiles and policy; each `agent::SessionHandle` owns an isolated agent tree
  and registry.
- Child agents are one-shot, profile-selectable agents. A child `ask` changes its stable agent job
  to `waiting_input`; answer that job with `job_send` (or `tool.job(id).send({value})` in a script).
  Root-agent questions still go directly to the host question handler.

Background-capable tools accept a common optional `bg` argument. Unclaimed completions and child
questions are retained, injected exactly once into their owning agent, and wake it for another turn.
On resume, unfinished jobs are marked
interrupted and job identifiers continue monotonically. Hosts can call `SessionHandle::interrupt` to
stop active provider streams and cancel jobs across the session's agent tree.

## Built-in tools

`read`, `search`, `glob`, `exec`, `shell`, `write`, `replace`, `patch`, `remove`, `script`, `targets`,
`target_add`, `jobs`,
`job_inspect`, `job_wait`, `job_send`, `job_cancel`, `job_events`, `ask`, `todo`, and `agent`.
Host-owned skills are exposed through `skills` and `skill`; they are discovered from the user
configuration directory and `.agents/skills` directories along the workspace ancestry.

## Development

```sh
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

Build both initial statically linked Linux shims and a release CLI with
`scripts/build-shims.sh`.

Skyhook is licensed under AGPL-3.0-only.

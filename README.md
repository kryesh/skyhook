# Skyhook

Skyhook is a provider-neutral coding-agent harness with a terminal interface and a
programmable JavaScript orchestration runtime. Use it interactively, run headless
workflows, or embed its Rust core in your own application.

[Documentation](https://kryesh.github.io/skyhook/) ·
[Book source](docs/src/SUMMARY.md) ·
[GitHub](https://github.com/kryesh/skyhook)

## What makes it different?

- **One tool interface, two ways to work.** Registered tools are available both as
  model tool calls and as lazy JavaScript builders. Scripts can batch independent
  work and coordinate more involved workflows through the same policy and job system.
- **Provider-neutral by design.** OpenAI Chat Completions and Responses, Anthropic
  Messages, and Codex/ChatGPT subscription providers share a common runtime API,
  including support for compatible local model endpoints.
- **Supervised jobs and agents.** Delegate to child agents, follow up on their work,
  and inspect saved results. Sessions retain conversations, job output, and model
  requests for later inspection and resumption.
- **Local and remote execution.** The same tools work in local workspaces and on Linux and
  Windows targets reached over SSH or WinRM, with explicit routing and credential ownership.

## Install

Requires **Rust 1.99 or newer**, [just](https://github.com/casey/just) and
[Zig](https://ziglang.org/download/) on `PATH`:

```sh
git clone https://github.com/kryesh/skyhook.git && cd skyhook
just setup     # Rust-side tooling and targets; names any missing build tool
just install   # release shims and both applications, into ~/.local/bin
```

The first build compiles V8 from source and takes a while. The
[installation guide](docs/src/getting-started/installation.md) covers the build tools it needs and
static builds.

Configure a provider and model, then start a session. If you already have a
configuration, keep it rather than downloading over it:

```sh
config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/skyhook"
mkdir -p "$config_dir"
curl -fsSL https://raw.githubusercontent.com/kryesh/skyhook/main/config.example.yaml \
  -o "$config_dir/config.yaml"
# Edit config.yaml: remove unused providers along with the models under them.
# Set credentials for every remaining provider; for OpenAI:
export OPENAI_API_KEY=...
~/.local/bin/skyhook --prompt "inspect this repository"
```

See [first session](docs/src/getting-started/first-session.md) and the
[configuration guide](docs/src/configuration/overview.md) for details.
For automation, see [headless execution](docs/src/guide/headless.md) and
[JavaScript workflows](docs/src/scripting/introduction.md).

For codebase security research, `skyhook-security` reuses your model configuration and the same
terminal and session runtime. It keeps a research ledger inside each session: components and the
paths through them, the flaws found there and the premises they rest on, candidate scenarios
built from those flaws, and the work they still owe; then parallel review, optional empirical validation, and reports. See
[security research](docs/src/guide/security.md) for configuration, runs, and reports.

## Development

See [AGENTS.md](AGENTS.md) for the repository's contribution policy and
[development setup](docs/src/development/setup.md) for prerequisites and build commands.
The [architecture](docs/src/development/architecture.md) and
[embedding](docs/src/development/embedding.md) chapters describe the Rust library.

Run the standard checks from the repository root:

```sh
just fmt
just lint
just test
```

For documentation changes, use `just docs` to build the book or `just docs-serve` to preview it.
See [documentation and publishing](docs/src/development/documentation.md) for writing guidance
and the separate, explicit GitHub Pages publishing step. Run `just` to list all recipes.

## License

[AGPL-3.0-only](LICENSE).

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
  Messages, and Codex/ChatGPT subscription backends share a common runtime API,
  including support for compatible local model endpoints.
- **Supervised jobs and agents.** Delegate to child agents, follow up on their work,
  and inspect saved results. Sessions retain conversations, job output, and model
  requests for later inspection and resumption.
- **Local and remote execution.** The same tools work in local workspaces and on SSH
  targets, with explicit routing and credential ownership.

## Install

Requires **Rust 1.88 or newer**. The default build embeds static Linux SSH shims for
x86_64 and aarch64, so it also requires [Zig](https://ziglang.org/download/) on `PATH`
and both Rust musl targets:

```sh
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
cargo install --git https://github.com/kryesh/skyhook skyhook-agent --locked
```

For a **local-only build**, without Zig or the extra targets:

```sh
cargo install --git https://github.com/kryesh/skyhook skyhook-agent --locked \
  --no-default-features --features tui
```

Configure a provider and model, then start a session. If you already have a
configuration, keep it rather than downloading over it:

```sh
config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/skyhook"
mkdir -p "$config_dir"
curl -fsSL https://raw.githubusercontent.com/kryesh/skyhook/main/config.example.toml \
  -o "$config_dir/config.toml"
# Edit config.toml: remove unused providers AND their model profiles.
# Set credentials for every remaining provider; for OpenAI:
export OPENAI_API_KEY=...
skyhook --prompt "inspect this repository"
```

See the [installation guide](docs/src/getting-started/installation.md),
[first session](docs/src/getting-started/first-session.md), and
[configuration guide](docs/src/configuration/overview.md) for details.
For automation, see [headless execution](docs/src/guide/headless.md) and
[JavaScript workflows](docs/src/scripting/introduction.md).

## Development

The workspace contains the `skyhook-agent` CLI package and
[`skyhook-agent-core`](crates/skyhook-core), whose Rust library crate is named
`skyhook`. The core owns the provider, tool, job, session, and agent runtimes.

Use [just](https://github.com/casey/just) for common tasks; run `just` to list them:

```sh
just build-local  # Debug CLI without embedded SSH shims; no Zig required
just test         # Core and CLI tests without rebuilding embedded shims
just lint         # Clippy with warnings denied
just fmt-check    # Check Rust formatting
just build        # Debug CLI with embedded shims; requires Zig and musl targets
just release      # Release CLI with embedded shims
just shim         # Native Linux SSH shim only
```

The documentation uses the installed [mdBook](https://rust-lang.github.io/mdBook/):

```sh
just docs         # Build docs/book
just docs-check   # Check local links and anchors (also requires Python 3.11+)
just docs-serve   # Preview the book locally
just pages-build  # Build the static site for the pages branch
```

See [development setup](docs/src/development/setup.md) for prerequisites and direct
Cargo commands, and the book's development section for architecture and publishing.
`just pages-publish REMOTE` builds and pushes the site to that remote's `pages`
branch (`REMOTE` defaults to `origin`). Publishing is explicit, not part of normal
builds; choose a remote that points to GitHub when deploying to GitHub Pages.

## License

[AGPL-3.0-only](LICENSE).

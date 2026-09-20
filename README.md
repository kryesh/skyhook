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

Model and JavaScript tool calls share a [JobView response contract](docs/src/reference/javascript.md#jobview-response-contract),
with native payloads in `.result`; the JavaScript surface also provides `response.unwrap()`.

## Install

Requires **Rust 1.88 or newer** and [just](https://github.com/casey/just). SSH support
embeds static Linux shims for x86_64 and aarch64, which are cross-compiled with
[Zig](https://ziglang.org/download/) (install it separately and put it on `PATH`):

```sh
git clone https://github.com/kryesh/skyhook.git && cd skyhook
just setup                 # cargo-zigbuild, cargo-nextest, mdBook, musl targets
just build-shims-release   # omit for a local-only build without SSH support
just build-release         # produces target/release/skyhook
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

The `skyhook-agent` package is a single library crate named `skyhook`, which owns the
provider, tool, job, session, and agent runtimes, plus two binaries: the `skyhook` CLI
(feature `tui`) and the `linux-ssh` remote shim (feature `shim-bin`).

Use [just](https://github.com/casey/just) for common tasks; run `just` to list them:

```sh
just setup               # Install cargo-zigbuild, cargo-nextest, mdBook, and musl targets
just build-shims         # Cross-compile debug SSH shims into target/shims (requires Zig)
just build-shims-release # Cross-compile size-optimised SSH shims into target/shims
just build               # Debug CLI using whatever is in target/shims
just build-release       # Release CLI embedding whatever is in target/shims
just test                # nextest suite plus doctests
just lint                # Clippy with warnings denied
just fmt                 # Format Rust code
```

The documentation uses the installed [mdBook](https://rust-lang.github.io/mdBook/):

```sh
just docs         # Build docs/book
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

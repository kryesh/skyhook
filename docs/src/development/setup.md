# Development setup

## Prerequisites

- Rust 1.88 or newer and Cargo. Install the toolchain's `rustfmt` and `clippy` components
  for formatting and lint checks.
- [just](https://just.systems/) on `PATH` for the repository task interface.
- Zig and the Rust targets `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`
  **only for builds embedding the SSH shims**, including normal debug builds.
- An installed [mdBook](https://rust-lang.github.io/mdBook/) for documentation work.
  Use the installed executable; the repository does not pin or bootstrap it. Python 3.11+
  is needed only for the documentation link checker, not for building the book.

```sh
git clone https://github.com/kryesh/skyhook.git
cd skyhook
just --list
```

For embedded-shim builds, ensure `zig` is on `PATH` (or set `CARGO_ZIGBUILD_ZIG_PATH`), then:

```sh
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
```

The build uses the `cargo-zigbuild` Rust library. A separate `cargo zigbuild` CLI and container
engine are not needed. See [remote transport and shims](remote-transport-and-shims.md) for
profiles, artifact paths, static validation, and the build matrix.

## Common tasks

Run these commands from the repository root:

| Command | Purpose |
| --- | --- |
| `just build` | Normal debug CLI, embedding both Linux SSH shims; requires Zig and both musl targets. |
| `just build-local` | Local-only debug CLI without embedding; no Zig or extra musl targets needed. |
| `just release` | Release CLI with both embedded shims. |
| `just shim` | Native Linux SSH shim only, without cross-compilation or UI dependencies. |
| `just test` | Core and CLI tests without embedding shims. |
| `just lint` | Workspace Clippy with warnings denied, without embedding shims. |
| `just fmt` | Format workspace Rust code. |
| `just fmt-check` | Check formatting without edits. |
| `just docs` | Build the mdBook into `docs/book`. |
| `just docs-check` | Build and check rendered local links/anchors and root README links. |
| `just docs-serve` | Serve and watch the book locally. |
| `just pages-build` | Build the Pages artifact and add `.nojekyll`. |
| `just pages-publish [remote] [branch]` | Publish a clean, committed checkout via a temporary worktree; defaults to `origin` and `pages`. |

Normal `cargo build`, `cargo test`, and debug installs use the default features and therefore
embed debug-profile shims. Prefer `just build-local`, `just test`, and `just lint` for local
iteration without that cross-build. A local-only CLI reports an explicit missing-shim error
if asked to use SSH. `just shim` alone does not create an embedded CLI catalog.

## Direct Cargo commands

The recipes are a convenience interface, not a replacement build system. Their corresponding
commands remain available:

```sh
cargo build --locked -p skyhook-agent
cargo build --locked -p skyhook-agent --no-default-features --features tui
cargo build --locked --release -p skyhook-agent
cargo build --locked -p skyhook-agent --bin linux-ssh --no-default-features --features shim-bin
cargo test --locked -p skyhook-agent-core --all-targets
cargo test --locked -p skyhook-agent --all-targets --no-default-features --features tui
cargo clippy --locked --workspace --all-targets --no-default-features --features tui -- -D warnings
cargo fmt --all
cargo fmt --all -- --check
```

The core package is `skyhook-agent-core` (Rust crate `skyhook`); the CLI package is
`skyhook-agent`. See [architecture](architecture.md) and [embedding](embedding.md) for the
library boundaries. [Documentation and publishing](documentation.md) covers local preview,
link validation, and the manual GitHub Pages workflow.

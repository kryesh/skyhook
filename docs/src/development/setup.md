# Development setup

## Prerequisites

- Rust 1.88 or newer via rustup, and [just](https://just.systems/) on `PATH`.
- [Zig](https://ziglang.org/download/) on `PATH` (or `CARGO_ZIGBUILD_ZIG_PATH`) **only for
  building the SSH shims**.

```sh
git clone https://github.com/kryesh/skyhook.git
cd skyhook
just setup
```

`just setup` adds the `rustfmt` and `clippy` components and both musl targets, and installs
`cargo-zigbuild`, `cargo-nextest`, and mdBook with `cargo install`. See
[remote transport and shims](remote-transport-and-shims.md) for shim profiles and artifacts.

## Common tasks

Run these commands from the repository root:

| Command | Purpose |
| --- | --- |
| `just setup` | Install the Rust-side tooling and shim targets. |
| `just build-shims` | Cross-compile debug SSH shims into `target/shims`; requires Zig. |
| `just build-shims-release` | Cross-compile size-optimised SSH shims into `target/shims`; requires Zig. |
| `just build` | Debug CLI; reads `target/shims` live at runtime. |
| `just build-release` | Release CLI embedding the current contents of `target/shims`. |
| `just build-full` | Release shims, then a release CLI that embeds them. |
| `just install` | `build-full`, then install the CLI as `~/.local/bin/skyhook`. |
| `just run [args]` | Run the debug CLI. |
| `just test` | Nextest suite over all targets and features. |
| `just lint` | Clippy over all targets and features with warnings denied. |
| `just fmt` | Format Rust code. |
| `just docs` | Build the mdBook into `docs/book`. |
| `just docs-serve` | Serve and watch the book locally. |
| `just pages-build` | Build the Pages artifact and add `.nojekyll`. |
| `just pages-publish [remote] [branch]` | Publish the built book via a temporary worktree, never force-pushing; defaults to `origin` and `pages`. |

There is no build script and no default feature: a plain `cargo build` compiles only the
library. Each binary requires its own feature, `tui` for `skyhook` and `shim-bin` for
`linux-ssh`, so pass `--features tui` (or `--all-features`) to direct Cargo commands.
Shims are only ever built by the `build-shims*` recipes; a CLI built without them reports an
explicit missing-shim error if asked to use SSH.

## Verification

Follow the repository policy in [AGENTS.md](https://github.com/kryesh/skyhook/blob/main/AGENTS.md).
The standard checks are:

```sh
just fmt
just lint
just test
```

When running Cargo directly, include the CLI/TUI and shim features:

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo nextest run --locked --all-targets --all-features
```

Nextest does not run doctests; run `cargo test --locked --doc --all-features` separately
when changing Rust documentation examples. For book changes, run `just docs` and check the
rendered pages and links. `AGENTS.md` also covers test policy, including stress runs for tests
that coordinate gated provider steps.

The `skyhook-agent` package is one library crate named `skyhook` plus those two binaries. See
[architecture](architecture.md) and [embedding](embedding.md) for the library boundaries.
[Documentation and publishing](documentation.md) covers local preview and manual GitHub Pages
publishing.

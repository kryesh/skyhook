# Development setup

## Prerequisites

- Rust 1.99 or newer via rustup, and [just](https://just.systems/) on `PATH`.
- [Zig](https://ziglang.org/download/) on `PATH` (or `CARGO_ZIGBUILD_ZIG_PATH`) **only for
  building the SSH shims**.
- `python3` on `PATH` for the test suite, whose MCP tests run a scripted server.

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
| `just build` | Debug builds of both applications; they read `target/shims` live at runtime. |
| `just build-release` | Release builds of both applications, embedding the current contents of `target/shims`. |
| `just build-full` | Release shims, then release applications that embed them. |
| `just install` | `build-full`, then install both applications into `~/.local/bin`. |
| `just run [args]`, `just run-security [args]` | Run a debug application. |
| `just test` | Nextest suite over the workspace and all targets. |
| `just lint` | Clippy over the workspace and all targets with warnings denied. |
| `just fmt` | Format Rust code. |
| `just docs` | Build the mdBook into `docs/book`. |
| `just docs-serve` | Serve and watch the book locally. |
| `just pages-build` | Build the Pages artifact and add `.nojekyll`. |
| `just pages-publish [remote] [branch]` | Publish the built book via a temporary worktree, never force-pushing; defaults to `origin` and `pages`. |

There is no build script and no cargo feature. A plain `cargo build` builds the two
applications; pass `-p linux-ssh` to build the shim.
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

When running Cargo directly, cover the whole workspace:

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo nextest run --locked --workspace --all-targets
```

Nextest does not run doctests; run `cargo test --locked --workspace --doc` separately
when changing Rust documentation examples. For book changes, run `just docs` and check the
rendered pages and links. `AGENTS.md` also covers test policy, including stress runs for tests
that coordinate gated provider steps.

The workspace holds the `skyhook` library, the `skyhook-cli` host with the `skyhook` binary, the
`skyhook-security` binary and the `linux-ssh` shim under `crates/`. See
[architecture](architecture.md) and [embedding](embedding.md) for the library boundaries.
[Documentation and publishing](documentation.md) covers local preview and manual GitHub Pages
publishing.

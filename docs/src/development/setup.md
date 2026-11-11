# Development setup

## Prerequisites

- Rust 1.99 or newer via rustup, and [just](https://just.systems/) on `PATH`.
- [Zig](https://ziglang.org/download/) 0.15 or newer on `PATH` (or `CARGO_ZIGBUILD_ZIG_PATH`) for release
  builds and the remote shims; debug builds and tests do not need it.
- `python3` on `PATH` for the test suite, whose MCP tests run a scripted server.

```sh
git clone https://github.com/kryesh/skyhook.git
cd skyhook
just setup
```

`just setup` adds the `rustfmt` and `clippy` components and every shim target, and installs
`cargo-zigbuild`, `cargo-nextest`, and mdBook with `cargo install`. It then checks for Zig and what
[release builds](#release-builds) need, and names whatever is missing. See
[remote transport and shims](remote-transport-and-shims.md) for shim profiles and artifacts.

## Release builds

`just build-release [target]` builds both applications into `target/<target>/release` on a Linux
host; only x86_64 hosts have been tested.

- Without `target`, it builds for this machine's own target (`just --evaluate native_target`
  prints it), using the host's C library and name resolution.
- `just build-release x86_64-unknown-linux-musl` or `aarch64-unknown-linux-musl` builds a static
  binary with musl's own resolver.

Targets of other architectures fail before building.

The first build for a `v8` version and target compiles V8 from source, taking several CPU-hours
and a few gigabytes under `target/v8`; later builds reuse it. To build offline, keep `target/v8`
and the cargo registry from an earlier build. The build needs, on `PATH`:

- `clang` and `clang++` at least as new as the clang major that V8 version's Chromium build pins,
  with their compiler-rt builtins; the build stops with the required version if the installed one
  is older;
- from the same LLVM, `ld.lld`, `llvm-ar`, `llvm-nm`, `llvm-readelf` and `llvm-objcopy`, and
  `llvm-config` for its `libclang.so`;
- `gn`, `ninja`, `git`, `python3`, and `unshare` (util-linux); `ccache` is used when present;
- a nightly Rust toolchain as this repository's active toolchain.

To upgrade V8, bump `v8` in `crates/skyhook/Cargo.toml`, run `cargo update -p v8`, and run
`just build-release` as usual.

## Common tasks

Run these commands from the repository root:

| Command | Purpose |
| --- | --- |
| `just setup` | Install the Rust-side tooling and shim targets, and check for the other build tools. |
| `just build-shims` | Cross-compile debug shims for every Linux and Windows shim target into `target/shims`; requires Zig. |
| `just build-shims-release` | Cross-compile size-optimised shims into `target/shims`; requires Zig. |
| `just build` | Debug builds of both applications; they read `target/shims` live at runtime. |
| `just build-release [target]` | Release builds of both applications in `target/<target>/release`, embedding the current contents of `target/shims`; see [release builds](#release-builds) for `target`. |
| `just build-full [target]` | Release shims, then release applications for `target` that embed them. |
| `just install [target]` | `build-full` for `target`, then install both applications into `~/.local/bin`. |
| `just run [args]`, `just run-security [args]` | Run a debug application. |
| `just test` | Nextest suite over the workspace and all targets. |
| `just lint` | Clippy over the workspace and all targets with warnings denied. |
| `just fmt` | Format Rust code. |
| `just docs` | Build the mdBook into `docs/book`. |
| `just docs-serve` | Serve and watch the book locally. |
| `just pages-build` | Build the Pages artifact and add `.nojekyll`. |
| `just pages-publish [remote] [branch]` | Publish the built book via a temporary worktree, never force-pushing; defaults to `origin` and `pages`. |

A plain `cargo build` builds the two applications; pass `-p skyhook-shim` to build the shim.
Shims are only ever built by the `build-shims*` recipes; an application built without them reports
an explicit missing-shim error when asked to connect to a remote target.

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

These checks compile and test for the build machine only. Windows code is compiled by
`just build-shims` alone, so run it after changing anything under `cfg(windows)`.

See [architecture](architecture.md#crate-layout) for the crates and
[documentation and publishing](documentation.md) for local preview and manual GitHub Pages
publishing.

# Installation

Skyhook is built from a source checkout with [just](https://github.com/casey/just). Rust 1.88 or
newer is required. SSH support needs static Linux shims, which are cross-compiled with
[Zig](https://ziglang.org/download/); install Zig yourself and make `zig` available on `PATH` (or
set `CARGO_ZIGBUILD_ZIG_PATH`). `just setup` installs the remaining Rust-side tooling
(`cargo-zigbuild`, `cargo-nextest`, mdBook) and both musl targets.

```sh
git clone https://github.com/kryesh/skyhook.git
cd skyhook
just setup
just build-shims-release   # stage static SSH shims into target/shims
just build-release         # target/release/skyhook, with the staged shims embedded
```

The executable is `skyhook`. Release builds embed whatever is in `target/shims` at compile time;
debug builds (`just build`) read that directory live. Skip the shim step for a local-only build:
SSH use from such a build returns an explicit missing-shim error. The `tui` feature enables the
`skyhook` binary and its UI dependencies; the same binary also supports headless execution.

See [development setup](../development/setup.md) for contributor commands and
[embedded shim builds](../development/remote-transport-and-shims.md#embedded-shim-builds)
for the build matrix, artifact layout, and native shim-only builds.

Continue with [your first session](first-session.md).

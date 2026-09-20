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
just install   # release shims, release CLI embedding them, installed as ~/.local/bin/skyhook
```

The executable supports both the terminal interface and headless execution; make sure
`~/.local/bin` is on `PATH`. For a local-only build without Zig, run `just build-release` instead
and use `target/release/skyhook`. SSH use without an included shim
returns an explicit missing-shim error.

See [development setup](../development/setup.md) for contributor commands and
[shim builds](../development/remote-transport-and-shims.md#building-and-packaging-artifacts)
for the build matrix, artifact layout, and native shim-only builds.

Continue with [your first session](first-session.md).

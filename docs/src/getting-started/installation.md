# Installation

## Full install with SSH support

Rust 1.88 or newer is required. Default installs build static Linux SSH shims from source,
including for debug builds. Install [Zig](https://ziglang.org/download/) and both Rust musl
targets first. Make `zig` available on `PATH`, or set `CARGO_ZIGBUILD_ZIG_PATH` to its executable:

```sh
zig version
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
cargo install --git https://github.com/kryesh/skyhook skyhook-agent --locked
```

The executable is `skyhook`. Installation uses the GitHub source repository, not crates.io.
The build support uses the `cargo-zigbuild` Rust library with your installed Zig; you do not
need the `cargo zigbuild` CLI, Cross, Docker, or Podman.

## Local-only install

Without Zig or the extra musl targets:

```sh
cargo install --git https://github.com/kryesh/skyhook skyhook-agent --locked --no-default-features --features tui
```

SSH use from this build returns an explicit missing-shim error. The `tui` feature enables
the interactive binary and its UI dependencies; the same binary also supports headless execution.

## Install from a checkout

```sh
git clone https://github.com/kryesh/skyhook.git
cd skyhook
cargo install --path . --locked
# Or local-only:
cargo install --path . --locked --no-default-features --features tui
```

See [development setup](../development/setup.md) for contributor commands and
[embedded shim builds](../development/remote-transport-and-shims.md#embedded-shim-builds)
for the build matrix, artifact layout, and native shim-only builds.

Continue with [your first session](first-session.md).

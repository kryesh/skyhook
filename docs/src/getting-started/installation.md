# Installation

Skyhook runs on Unix and is built from a source checkout with [just](https://github.com/casey/just).
Rust 1.99 or newer is required. Release builds are made on Linux: static musl binaries that embed static
Linux SSH shims; both are cross-compiled with [Zig](https://ziglang.org/download/). Install Zig
yourself and make `zig` available on `PATH` (or set `CARGO_ZIGBUILD_ZIG_PATH`). `just setup`
installs the remaining Rust-side tooling (`cargo-zigbuild`, `cargo-nextest`, mdBook), the shim
targets, and this machine's release target.

Release builds also compile the V8 JavaScript engine from source on their first run, which needs
a recent system `clang`, `gn`, `ninja` and a nightly Rust toolchain; see
[release builds](../development/setup.md#release-builds) for the exact requirements and costs.

```sh
git clone https://github.com/kryesh/skyhook.git
cd skyhook
just setup
just install   # release shims and both applications, installed into ~/.local/bin
```

This installs `skyhook`, the general-purpose coding agent, and `skyhook-security`, the
[security research companion](../guide/security.md). Both support the terminal interface and
headless execution and embed the same SSH shims. Make sure `~/.local/bin` is on `PATH`.

For a local-only build without Zig, run `cargo build --locked --release -p cli -p security`
instead and use `target/release/skyhook` or `target/release/skyhook-security`.
`just build` builds both applications in `target/debug`. SSH use without an included shim returns
an explicit missing-shim error.

Both applications use the ordinary Skyhook provider configuration and credentials. The companion
also reads `security.yaml`; use `security.example.yaml` as a starting point and adjust its model
assignments to match your `config.yaml`. See [security configuration](../guide/security.md#configuration)
for discovery rules and research settings.

See [development setup](../development/setup.md) for contributor commands and
[shim builds](../development/remote-transport-and-shims.md#building-and-packaging-artifacts)
for the build matrix, artifact layout, and native shim-only builds.

Continue with [your first session](first-session.md).

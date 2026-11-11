# Installation

Skyhook runs on Unix and is built on Linux from a source checkout. It needs Rust 1.99 or newer,
[just](https://github.com/casey/just), [Zig](https://ziglang.org/download/) on `PATH`, and the
build tools listed under [release builds](../development/setup.md#release-builds). `just setup`
installs the Rust-side tooling and names anything else that is missing.

```sh
git clone https://github.com/kryesh/skyhook.git
cd skyhook
just setup
just install   # release shims and both applications, installed into ~/.local/bin
```

The first build compiles the V8 JavaScript engine from source, which takes several CPU-hours.

This installs `skyhook`, the general-purpose coding agent, and `skyhook-security`, the
[security research companion](../guide/security.md). Both support the terminal interface and
headless execution and embed the same shims. Make sure `~/.local/bin` is on `PATH`.

`just install` builds for this machine's own target, against its C library.
`just install x86_64-unknown-linux-musl` installs a static build instead.

For a local-only build without Zig, run `cargo build --locked --release -p skyhook-apps`
instead and use `target/release/skyhook` or `target/release/skyhook-security`.
`just build` builds both applications in `target/debug`. Connecting to a remote target without an
included shim returns an explicit missing-shim error.

Both applications use the ordinary Skyhook provider configuration and credentials. The companion
also reads `security.yaml`; use `security.example.yaml` as a starting point and adjust its model
assignments to match your `config.yaml`. See [security configuration](../guide/security.md#configuration)
for discovery rules and research settings.

See [development setup](../development/setup.md) for contributor commands and
[shim builds](../development/remote-transport-and-shims.md#building-and-packaging-artifacts)
for the build matrix, artifact layout, and native shim-only builds.

Continue with [your first session](first-session.md).

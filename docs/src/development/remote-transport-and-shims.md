# Remote transport and shims

## Remote transport

A shim executes individual tool operations on its machine. Job state, saved output, and image
attachments belong to the host; output streams back while requests run. The shim does not create
a session database. Connections are pooled per workspace.

SSH configuration is generated only from target definitions. An SSH process runs on root or on
the shim of the destination's `origin`; `via` hops are native `ProxyJump` hops of that process.
Cancelling one waiter never terminates a connection startup shared with other callers.

## Embedded shim builds

Shims are built explicitly, never as a side effect of building the CLI. `just build-shims` builds
every shim type with Cargo's `dev` profile; `just build-shims-release` uses the size-optimised,
stripped `shim-release` profile. The `linux-ssh` shim is cross-compiled with `cargo zigbuild` for
every target in the justfile's `linux_ssh_targets` and copied to `target/shims/linux-ssh-<arch>`,
replacing the previous variant. New shim types are added to the justfile's `_shims` recipe. Debug
artifacts are substantially larger. Zig 0.16.0 was used to validate both targets; `just setup`
installs `cargo-zigbuild` and the musl targets, but Zig itself must be installed separately.

The CLI embeds whatever `target/shims/` contains, via `rust-embed`, without checking how it got
there. Debug builds (`just build`) read the directory live at runtime, so rebuilding shims needs
no CLI rebuild. Release builds (`just build-release`) include the bytes at compile time and are
self-contained; the recipe touches the embedding module first because `rust-embed` cannot detect
newly added files. A missing or empty directory yields an empty catalog, and SSH use then
reports an explicit missing-shim error.

## Shim targets and artifacts

| Platform | Protocol | Architecture | Rust target | Artifact |
| --- | --- | --- | --- | --- |
| `linux` | `ssh` | `x86_64` | `x86_64-unknown-linux-musl` | `target/shims/linux-ssh-x86_64` |
| `linux` | `ssh` | `aarch64` | `aarch64-unknown-linux-musl` | `target/shims/linux-ssh-aarch64` |

Git contains source only, not prebuilt shim binaries. To build just a native Linux SSH shim
without Zig or UI dependencies:

```sh
cargo build --bin linux-ssh --features shim-bin
```

See [development setup](setup.md) for the full recipe list. Library hosts inject their own
[embedded catalog](embedding.md#embedded-shim-catalog).

Source: [remote module](https://github.com/kryesh/skyhook/tree/main/src/remote)
and [shim binary](https://github.com/kryesh/skyhook/tree/main/src/bin/linux-ssh).

# Remote transport and shims

## Remote transport architecture

Remote connection protocols use the production `ConnectionFactory` interface in
`src/remote/backend.rs`. Factories receive the route hops after the destination's origin and return an owned
`Transport` byte stream; the manager performs the common shim handshake and client setup for both
real backends and test doubles. Protocol selection goes through the backend dispatch layer, rather
than constructing SSH launchers in the manager or target router.

- `remote/manager.rs` owns splitting routes at SSH origins, workspace connection pooling,
  cancellation, and invalidation.
- `remote/client/` owns shared shim RPC and relayed stream state; `remote/transport.rs` defines
  the owned asynchronous byte-stream contract.
- `remote/backends/ssh/` contains OpenSSH configuration/bootstrap, process supervision, askpass,
  and session-owned authentication. The public `remote::ssh` path remains a compatibility export.
- `remote/protocol.rs` is the **shim wire protocol**, not the SSH/WinRM transport interface.

SSH configuration is generated only from target definitions. An SSH process runs on root or on
the shim of the destination's `origin`, whose connection the child stream retains; `via` hops are
native `ProxyJump` hops of that process and share its origin. Each origin runs its own lazy private
agent. A backend must release its resources during session shutdown. Cancelling one waiter must not terminate a connection startup shared with
other callers. Adding another protocol still requires its own target configuration,
authentication, and bootstrap; the abstraction does not make SSH shell commands or ProxyJump
semantics universal.

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

The runtime catalog is not tied to the build targets: it discovers embedded filenames and selects
by protocol, platform, and architecture after probing the remote machine. It also understands
executable extensions, such as `windows-winrm-x86_64.exe`, but no Windows build target or WinRM
transport is implemented yet.

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

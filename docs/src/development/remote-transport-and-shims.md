# Remote transport and shims

## Remote transport architecture

Remote connection protocols use the production `ConnectionFactory` interface in
`crates/skyhook-core/src/remote/backend.rs`. Factories receive the route hops after the destination's origin and return an owned
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

Build both statically linked Linux SSH shims and the release CLI with
`cargo build --release -p skyhook-agent`. This default-feature build requires Zig and both Rust
musl targets listed below; it does not require a container engine. Zig 0.16.0 was used to validate
both targets.

Debug builds (`cargo build`, `cargo test`, and `cargo install --debug`) build the shims with
Cargo's `dev` profile: no optimization or stripping by default, with debug information and
debug assertions. Release builds keep the size-optimized, stripped `shim-release` profile.
Both modes use the same musl targets and static ELF validation, and publish to the same artifact
filenames in `target/shims/`. Switching build modes replaces those files with the matching variants;
debug artifacts and the binaries embedding them are substantially larger.

The private build matrix is `ShimArchitecture` in [`build.rs`](https://github.com/kryesh/skyhook/blob/main/build.rs).
Its single architecture definition generates the supported variants and their architecture labels,
Rust target triples, and expected ELF machine codes. All built-in shims use the `linux-ssh` Cargo
binary; the saved artifacts remain `linux-ssh-x86_64` and `linux-ssh-aarch64`. This closed build
matrix does not restrict the runtime catalog: it discovers embedded filenames and selects by
protocol, platform, and architecture after probing the remote machine. The catalog also understands executable extensions, such as
`windows-winrm-x86_64.exe`, but no Windows build target or WinRM transport is implemented yet.

`rust-embed` embeds both the file list and bytes in debug and release builds, so installed binaries
do not need the build-time `target/shims/` directory. Local-only and rust-analyzer builds skip embedding even
if artifacts already exist. Shim builds deliberately ignore host Rust flags (including Cargo
configuration rustflags) to keep the artifacts portable.

The fixed staging location is `<package>/target/shims/`, even when `CARGO_TARGET_DIR` or
`--target-dir` redirects compilation elsewhere. It must be writable; intermediate shim builds
remain isolated under Cargo's `OUT_DIR`. The embedder allows a missing staging directory for
clean-tree tooling, but normal embedding builds must successfully generate and validate their
shims before the catalog is enabled.

Generated files are published atomically and unchanged bytes are not rewritten; unrelated
artifacts are preserved. Remove obsolete artifacts explicitly when changing the build matrix,
and avoid simultaneous builds writing to the same staging directory.

Cargo excludes the package's `target/` directory from source packages and package-verification
source comparisons. Staging there therefore avoids the source-modification failure of the old
source-root `shims/` location, without requiring local-only verification flags:

```sh
cargo build --release -p skyhook-agent --locked
cargo package -p skyhook-agent --locked
```

## Shim targets and artifacts

The `linux-ssh` binary is built from `src/bin/linux-ssh` for these shim targets:

| Platform | Protocol | Architecture | Rust target |
| --- | --- | --- | --- |
| `linux` | `ssh` | `x86_64` | `x86_64-unknown-linux-musl` |
| `linux` | `ssh` | `aarch64` | `aarch64-unknown-linux-musl` |

The resulting static ELF executables are validated, written to the gitignored generated-artifact
folder as `target/shims/linux-ssh-x86_64` and `target/shims/linux-ssh-aarch64`, and embedded in the installed
`skyhook` command with `rust-embed`. Git and the Cargo source package contain source only, not
prebuilt shim binaries.

The build support uses the `cargo-zigbuild` Rust library with the installed Zig executable.
Install prerequisites as described in [installation](../getting-started/installation.md).
To build just the native Linux SSH shim without Zig or UI dependencies:

```sh
cargo build --bin linux-ssh --no-default-features --features shim-bin
```

The contributor interface provides `just build`, `just build-local`, `just release`, and
`just shim`; see [development setup](setup.md). Library hosts inject their own
[embedded catalog](embedding.md#embedded-shim-catalog).

Source: [remote module](https://github.com/kryesh/skyhook/tree/main/crates/skyhook-core/src/remote)
and [shim binary](https://github.com/kryesh/skyhook/tree/main/src/bin/linux-ssh).

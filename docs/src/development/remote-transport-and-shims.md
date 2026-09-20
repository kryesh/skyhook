# Remote transport and shims

Remote execution changes where an operation runs, not who owns the agent or job. Provider calls,
agent loops, policy decisions, job state, saved output, and session blobs remain owned by the
Skyhook host. A shim runs individual operations and streams their output back; it does not create
a session database or run a second agent harness.

## Responsibility boundaries

- Target resolution selects a validated route and execution workspace. SSH configuration comes
  from target definitions, not the user's or system's SSH configuration.
- `RemoteManager` owns the session's connection pool, keyed by resolved route identity **and**
  workspace. It coordinates shared startup, invalidation, and shutdown.
- `ConnectionFactory` supplies the byte stream and lifetime owner. The SSH backend handles
  authentication, OpenSSH processes, platform probing, and shim deployment; it does not implement
  job supervision or saved-output storage.
- The common client performs the shim handshake and routes multiplexed requests, output,
  authorization checks, and private control messages. The host persists streamed output and
  images using the same job-output machinery as local execution.
- The worker reuses the local built-in invocation layer for argument admission and operation
  execution. Permission requests return to the host's authorization coordinator. Private services
  support relayed SSH streams and sensitive prompts without turning them into tool results.

This division keeps transport-specific code below the tool/job boundary and lets local and remote
operations share validation and execution behavior.

## Connection and request lifecycle

Connection startup is shared by callers using the same pool key. Cancelling one waiter stops its
wait, not the shared startup. Startup failure removes the failed slot; I/O or protocol failure
during execution discards the affected pooled connection so later work can establish a new one.
It does not transparently replay the failed tool operation.

Once connected, request IDs associate output and cancellation with individual operations.
Credit-based flow control bounds queued payload and relayed-stream data. A transport's lifetime
owner retains its underlying process resources; dropping it tears those resources down. A relayed
connection also retains its origin connection for the lifetime of the child stream. Session
shutdown cancels startup, clears the pool, and releases session-owned authentication resources.

### Origin versus jump hosts

[Routing](../guide/execution-targets.md#routing-origin-and-via) decides where SSH runs: on root
or on the shim of the destination's `origin`. Targets listed through `via` are native `ProxyJump`
hops of that SSH process, not additional shim workers. A remote origin's shim can start SSH and relay its
byte stream over the existing host connection, while the host still runs the destination's
protocol client and supervises its jobs.

### Shim deployment

The backend probes the remote platform, selects a matching embedded artifact, and installs it in
a digest-named directory under the remote user's cache. An existing executable is reused only
after it matches the expected digest, and an upload is verified before it is installed. The shim starts in the
requested workspace. The target's configured workspace
is separately resolved as the authorization root, so a per-call working directory does not
silently redefine the target's permission boundary.

## Authentication and prompt isolation

Each credential origin can lazily own a private `ssh-agent`. Root's agent is session-owned; remote
origins manage theirs inside the shim. [`external_agent`](../guide/execution-targets.md#agents)
targets use the origin's inherited agent socket instead. Ordinary processes
started by a remote worker receive OpenSSH's forwarded `SSH_AUTH_SOCK`; no second forwarding
listener is needed.

SSH authentication prompts use `SensitivePromptHandler`, separate from model questions and tool
output. Remote prompts and answers travel as private control messages, not session events or
saved results. OpenSSH is given an explicit askpass helper so the host can handle passwords,
passphrases, and confirmations without exposing them to the agent transcript.

Each askpass server keeps its helper and socket in a private temporary directory and accepts
only peers running as its own user. This prevents other local users from injecting
prompts or obtaining answers through the socket; it is not an isolation boundary against other
processes running as the same user. Dropping the server cancels its task and removes its temporary
files.

## Building and packaging artifacts

Shims are built explicitly, never as a side effect of building the CLI. `just build-shims` builds
every shim type with Cargo's `dev` profile; `just build-shims-release` uses the size-optimised,
stripped `shim-release` profile. The `linux-ssh` shim is cross-compiled with `cargo zigbuild` for
every target in the justfile's `linux_ssh_targets` and staged into `target/shims/`, replacing the
previous variant. New shim types belong in the justfile's `_shims` recipe. `just setup` installs
`cargo-zigbuild` and the musl targets; Zig itself must be installed separately.

| Platform | Protocol | Architecture | Rust target | Artifact |
| --- | --- | --- | --- | --- |
| `linux` | `ssh` | `x86_64` | `x86_64-unknown-linux-musl` | `target/shims/linux-ssh-x86_64` |
| `linux` | `ssh` | `aarch64` | `aarch64-unknown-linux-musl` | `target/shims/linux-ssh-aarch64` |

The CLI discovers staged artifacts through `rust-embed`, without checking which build profile
produced them. Debug builds (`just build`) read the directory live, so rebuilding shims needs no
CLI rebuild. Release builds (`just build-release`) include bytes at compile time and are
self-contained. That recipe touches the embedding module first because `rust-embed` cannot detect
newly added files. Debug shim artifacts are substantially larger than release artifacts.

A missing or empty staging directory yields an empty catalog; SSH then reports an explicit
missing-shim error. Library hosts inject their own [embedded catalog](embedding.md#embedded-shim-catalog)
rather than inheriting the CLI catalog.

Git contains source only, not prebuilt shim binaries. To build a native Linux SSH shim without
Zig or UI dependencies:

```sh
cargo build --bin linux-ssh --features shim-bin
```

This Cargo command builds the binary but does not stage it into `target/shims/`. See
[development setup](setup.md) for the recipe list, including the combined shim and CLI build.

Source: [remote module](https://github.com/kryesh/skyhook/tree/main/src/remote),
[shim binary](https://github.com/kryesh/skyhook/tree/main/src/bin/linux-ssh),
[CLI catalog](https://github.com/kryesh/skyhook/blob/main/src/bin/skyhook/embedded_shims.rs), and
[build recipes](https://github.com/kryesh/skyhook/blob/main/justfile).

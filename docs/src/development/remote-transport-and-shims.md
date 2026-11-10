# Remote transport and shims

Remote execution changes where an operation runs, not who owns the agent or job. Provider calls,
agent loops, policy decisions, job state, saved output, and session blobs remain owned by the
Skyhook host. A shim runs individual operations and streams their output back; it does not create
a session database or run a second agent harness.

## Responsibility boundaries

- Target resolution selects a validated route and execution workspace. SSH configuration comes
  from target definitions, not the user's or system's SSH configuration.
- `RemoteManager` owns the session's connection pool: one connection per route, shared by calls
  in every workspace along it. It coordinates shared startup, invalidation, and shutdown.
- `ConnectionFactory` supplies the byte stream and lifetime owner. The backend handles SSH
  authentication and OpenSSH processes, WinRM connections, platform probing, and shim deployment;
  it does not implement job supervision or saved-output storage.
- The common client performs the shim handshake and runs typed operations on the shim's machine.
  The host's tools render their events into the job's output exactly as they render a local
  call's.
- The worker (`skyhook-worker`) performs operations and reports typed facts; it runs no tool,
  renders no text and asks for no permission. A relayed SSH or WinRM stream is one of its
  operations. The shim binary itself only reads its arguments, checks its own hash, prunes the
  shims its install replaced and serves one connection.

This division keeps transport-specific code below the tool/job boundary: a tool runs the same
operation whether its target is the session host or a connection.

## Protocol

A host and a shim exchange typed frames over one byte stream, the shim's standard input and
output.

- **Handshake.** The client's first frame is `Hello`, and the worker either reports what it is
  (its platform, time zone, and the authorization root it resolved) or refuses, naming the
  setting it cannot use.
- **Calls.** Every operation is a call. Its ID associates its events, input, cancellation and end,
  so any number of calls share the connection as independent streams, and none waits for another.
- **Flow control.** A call's events and its input each run at most one window of frames ahead of
  their acknowledgements, so a slow reader at either end bounds what the other buffers for that
  call alone.
- **Large frames.** A frame too large to send whole, such as a large inline write, travels in
  pieces on its call's stream, other calls' frames between them. No tool's data is limited by a
  frame's size, and no call's large frame holds up another's.
- **Workspaces.** The first call in a workspace has the worker resolve it (the `Workspace`
  operation), once for the connection: it joins a job's workspace as written to the target's, and
  refuses one it cannot use as it would the connection's root. The target's configured workspace
  is separately the authorization root, so a per-call working directory never redefines the
  target's permission boundary.
- **Contained calls.** A call the host approved in advance for paths inside the authorization
  root carries the paths it names. The worker resolves them, and when every one resolves inside
  the root it performs the operation on those resolutions in that same call. Otherwise it holds
  the operation and answers with the resolutions; the host approves the call as any other, then
  resumes or cancels it. Containment is the one decision a worker makes for the host.
- **Ending.** Dropping a call cancels it on the worker and takes back what the client had not
  begun to send of it; a call whose request was not yet sent is never sent. The client ends a
  connection, and the worker stops that connection's calls and acknowledges the end only once
  all their work has returned and the processes they started are killed, so nothing a connection
  started is still changing files when its end is acknowledged or the shim exits. A worker whose
  client departs without ending, or whose output fails, stops its calls the same way.

A peer that breaks this contract, for example with a contained call whose operation names other
paths than those it resolves, fails the connection with a typed `protocol::Violation`.

## Connection lifecycle

Connection startup is shared by the callers of a route. Cancelling one waiter stops its wait, and
the last waiter to leave stops the startup; redefining a target on the route stops it too, and its
callers plan again. Transport or protocol failure during execution discards the pooled connection
so later work establishes a new one. It does not transparently replay the failed tool operation;
output a tool already captured remains inspectable.

Output too large for the session stays on the target. A shim keeps a fetched body over 4 MiB, and
a command's stream past its first 4 MiB, in files under `~/.cache/skyhook/spill/<session id>/`
(`$XDG_CACHE_HOME` when set; `%LOCALAPPDATA%\skyhook\spill\<session id>\` on Windows), beside its
own `shims` directory. The host chooses the limit and each file's name; the shim creates the
directory for its user alone, stages the file and publishes it only whole, and reports it as it
resolved there; a directory whose resolved path is not Unicode keeps nothing. Each write to a
staged file is blocking work the connection counts, so none outlives the connection's end. A
write its file took before a deadline ended the command's reading is in the file published and
in the counts reported with it; if that write fails, the stream is lost.
While a command whose stream it is keeping runs,
the shim reports the stream's latest bytes and the lines counted so far: at most the tail it
reports at the end, at most as often as the host asks, and only when the report before was
taken, so a slow connection never holds the command's output up. A stream that ends while its
command runs on is reported once more, and that report waits to be taken. A read of a text file
past the host's limit counts the file's bytes and lines before it sends any, stopping at a NUL
byte, at bytes that are not UTF-8 or once the read is dropped, so the host learns the size of a
text file it leaves where it is. A later
page of such a file, or of a stream the shim kept, is a read operation that starts at the page's
first line, which the shim finds, so nothing before that line crosses the connection; the shim
stops looking for the line once the page is dropped. A read that needs the whole field reads the
file from its start.

Dropping the last owner of a pooled connection ends it. Session shutdown cancels startups, admits
no new ones, and ends each connection once the connections its shim carries as their origin have
ended, so a shim finishes ending, killing the SSH processes it started and deleting the shells it
opened, before the origin's end stops the stream it runs on. It waits for startup and connection
work before the backend releases session-owned authentication resources, and is done once the
SSH processes and shells root itself started are killed and deleted.

### Origin versus jump hosts

[Routing](../guide/execution-targets.md#routing-origin-and-via) decides where a connection
starts: on root or on the shim of the destination's `origin`. Targets listed through `via` are
native `ProxyJump` hops of that SSH process, not additional shim workers; a WinRM destination has
none and is never one. A remote origin's shim can start SSH, or run a command over its own WinRM
client, and relay the stream over the existing host connection, while the host still runs the
destination's protocol client and supervises its jobs. Deployment travels that stream too: the
host probes the next hop and uploads its shim from its own catalog, so an origin needs no shim
binaries. A relayed connection retains its origin connection for as long as it lives.

A WinRM stream is a command in a WinRS shell: its standard input and output carry the bytes, as
an SSH session's do, and once it ends its exit status and the start of its error output explain
a failure, as SSH's own diagnostics do. Dropping the stream deletes its shell, one still being
created once the service has made it, and a connection's end and a session's shutdown each wait
for the shells they opened to be deleted. That wait has one bound from the drop, which what was
left of a shell's creation and its deletion share, and it is the time a host allows an origin's
end beyond its own.

### Shim deployment

The target's declared `os` chooses the language of the commands that deploy and start its shim:
POSIX sh, or Windows PowerShell 5.1 started from cmd.exe, the shell of both Windows OpenSSH and
WinRS, so one set of commands serves both transports. The probe's answer is checked against the
declaration: a shell that runs the declared system's commands and names the other system, as a
POSIX shell on Windows does, fails the connection before anything is installed. An installed shim
needs no such check, since the digest in its name is that of a shim built for the declared `os`.

A connection starts with one command, which starts the first shim of the declared `os` that is
installed and matches its digest, and otherwise prints a probe of the platform. A shim writes a
fixed preamble before its first frame, so the host tells a started shim from a probe and skips
whatever the shell printed before it, such as a login script's output. After a probe, the host
uploads the platform's shim, which is verified before it is installed, and starts it: a
connection takes one command once its shim is installed and three before.

A shim is installed as a digest-named file: `~/.cache/skyhook/shims/<sha256>` (under `$XDG_CACHE_HOME` when that is set), or on Windows
`%LOCALAPPDATA%\skyhook\shims\<sha256>.exe`. A shim that starts serving removes the other shims
there built for its platform that no connection has checked for a day, and the uploads an
interrupted install left there a day ago or more.

## Authentication

Each credential origin authenticates with the agent it inherited: root's own `SSH_AUTH_SOCK`, or
the agent forwarded to a remote origin's shim. Only an origin without one lazily owns a private
`ssh-agent`, which it starts when a route loads a key or forwards an agent; a route offering only
agent keys uses it only while it already runs. Root's private agent is session-owned; remote
origins manage theirs inside the shim. Once the session shuts down, its agent is never started
again. A connection forwards its agent only for a `forward_agent` target. A Windows origin starts
no private agent; see [agents](../guide/execution-targets.md#agents).

A private agent's socket is made in a private directory under `XDG_RUNTIME_DIR` when that is set
and writable, otherwise under the temporary directory, and under `/tmp` when neither gives a path
short enough to bind.

Hops without a password run in `BatchMode`, so a connection that would need a password, a
keyboard-interactive answer, a key passphrase or a host-key confirmation fails, and OpenSSH's own
reason is the connection error. An SSH process has at most one hop with a
[password](../guide/execution-targets.md#passwords): the registry refuses a target whose
connection, the target and its `via` chain up to its origin, would need two, since telling two
prompts on one terminal apart would mean reading their text, and the route an origin receives
cannot express two.

The hop with the secret offers only OpenSSH's password method (with `key`, only that key), so
no server writes to the terminal its process is given. Skyhook types the password once, at the
terminal's first output, without reading it; a hop that never prompts is never typed. That output is always the secret's prompt,
since no hop asks about a host key: Skyhook writes each hop's `StrictHostKeyChecking` and
`UpdateHostKeys` from the target's options, `ask` becoming `yes` and `no`, and user and system
configuration set neither. Output after the answered line kills the process and fails the
connection, reporting the answered prompt. A root origin reads the password once per connection
attempt; a remote origin reads it for each SSH process it starts.

A WinRM connection reads its [password](../guide/execution-targets.md#winrm-targets), and a `ca`
file or Kerberos configuration, on the machine that connects: once per connection attempt on
root, and for each relayed command on a remote origin, never kept.

Processes Skyhook starts for tools get no terminal: a command that asks for a password, such as
`sudo` without cached credentials, fails by itself. Askpass and display variables Skyhook
inherited reach those processes unchanged.

## Building and packaging artifacts

Shims are built explicitly, never as a side effect of building the applications. `just build-shims`
builds every shim with Cargo's `dev` profile; `just build-shims-release` uses the size-optimised,
stripped `shim-release` profile. One `shim` binary per platform serves every protocol that
platform uses. It is cross-compiled with `cargo zigbuild` for every target in the justfile's
`shim_targets` and staged into `target/shims/` as `<os>-<arch>`, replacing the previous variant.
`just setup` installs `cargo-zigbuild` and the shim targets; Zig itself must be installed
separately.

| Platform | Architecture | Rust target | Artifact |
| --- | --- | --- | --- |
| `linux` | `x86_64` | `x86_64-unknown-linux-musl` | `target/shims/linux-x86_64` |
| `linux` | `aarch64` | `aarch64-unknown-linux-musl` | `target/shims/linux-aarch64` |
| `windows` | `x86_64` | `x86_64-pc-windows-gnu` | `target/shims/windows-x86_64` |

The applications discover staged artifacts without checking which build profile produced them.
Debug builds (`just build`) read the directory live, so rebuilding shims needs no rebuild of the
applications; a running application keeps one read of each shim. A staged shim built from
other sources than the application fails its connection at the first frame either end cannot
decode, so rebuild the shims after changing the protocol. Release builds (`just build-release`)
include the bytes at compile time and are self-contained. Debug shim artifacts are substantially
larger than release artifacts.

Of the staged files named `*-*`, one with an extension is skipped, and any other must name a
supported platform as `<os>-<arch>` or the catalog fails. A missing or empty staging directory yields
an empty catalog; connecting to a remote target then reports an explicit missing-shim error,
distinct from an unsupported remote platform. Library hosts inject their own
[embedded catalog](embedding.md#embedded-shim-catalog) rather than inheriting the binaries'
catalog.

Git contains source only, not prebuilt shim binaries. To build a native shim without Zig or UI
dependencies:

```sh
cargo build -p skyhook-shim
```

This Cargo command builds the binary but does not stage it into `target/shims/`. See
[development setup](setup.md) for the recipe list, including the combined shim and application
build.

Source: [remote client and deployment](https://github.com/kryesh/skyhook/tree/main/crates/skyhook/src/remote),
[protocol and serve loop](https://github.com/kryesh/skyhook/tree/main/crates/worker/src/protocol),
[SSH and WinRM origin](https://github.com/kryesh/skyhook/tree/main/crates/worker/src/origin),
[shim binary](https://github.com/kryesh/skyhook/tree/main/crates/shim),
[embedded catalog](https://github.com/kryesh/skyhook/blob/main/crates/apps/src/shims.rs), and
[build recipes](https://github.com/kryesh/skyhook/blob/main/justfile).

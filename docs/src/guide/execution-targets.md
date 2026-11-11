# Execution targets

Include `"targets"` in a [mode](permissions.md#modes)'s `capabilities` (or a batch job's
`--capabilities` allowlist) to grant the target capability. It is not enabled by default.
Without it, agents cannot use target-management tools or select another target, including from
JavaScript.

Targets are named entries in the top-level `targets` mapping, and session tools can add or replace
targets without modifying YAML. Skyhook never reads SSH configuration files: a target's host, user, port,
authentication, and options come only from its own definition. Aliases, `ProxyJump`, identities,
and other settings in `~/.ssh/config` or `/etc/ssh/ssh_config` do not apply.

When [user and workspace config are layered](../configuration/overview.md#file-selection-and-layering),
a workspace entry replaces the **whole target with the same name**; it does not inherit user SSH,
authentication, or routing settings. Targets with other names are retained. Repeat any
required settings in the replacement entry.

```yaml
targets:
  bastion:
    host: "bastion.example.com"
    transport:
      ssh:
        user: "gateway"

  build:
    host: "build.internal"
    workspace: "/srv/project"
    via: "bastion"
    transport:
      ssh:
        key: "~/.ssh/build_ed25519"
        password: {env: "BUILD_KEY_PASSPHRASE"}
        forward_agent: true

  legacy:
    host: "legacy.internal"
    transport:
      ssh:
        user: "deploy"
        password: {command: ["pass", "show", "legacy"]}

  database:
    host: "db.internal"
    origin: "build"
    transport:
      ssh:
        # Reach the host through a gateway using the deploy user's own SSH key on build.
        options: { ProxyCommand: "sudo -n -u deploy ssh -W %h:%p gateway.internal" }
```

The built-in `root` is the session host—the machine running the main Skyhook process. It is not
configured as a target and always identifies that machine, including when an agent's current
target is remote. Named targets give their transport and its settings under `transport`: `ssh`, or
`winrm` (see [WinRM targets](#winrm-targets)); an SSH target with no settings is written
`transport: {ssh: {}}`. A target's
`os` declares its operating system, `"linux"` or `"windows"`, by default `"linux"` for `ssh` and
`"windows"` for `winrm`; see [Windows targets](#windows-targets). Skyhook starts the target with
the commands of the system declared, so declare it correctly. A target whose shell runs them and
names the other system, such as a Windows host whose SSH default shell is a POSIX one declared
`"linux"`, fails to connect with `the target declares os linux but runs windows`; one whose
shell cannot run them fails with that shell's own error.

## Routing: `origin` and `via`

- `origin` names the target that starts the SSH or WinRM connection; it defaults to `root`.
  Key paths, CA files, password sources and agents belong to the origin.
- `via` names an SSH target to jump through with native `ProxyJump`, within that same
  connection. Jump hosts need no shim. A `via` target must have the same `origin`, so its key
  paths are never interpreted on another machine. Omit `via`, rather than naming the origin, to
  connect directly from the origin. A WinRM target is never a jump and takes no `via`: it
  connects directly from its origin.
- One SSH connection types at most one [password](#passwords): of a target and the targets it
  reaches through `via`, only one may set `password`. A route needing two is refused when the
  target is configured or added. Use `origin` instead of `via` for the later of the two, or a
  target between them: every target can be an origin, and each origin starts SSH connections of
  its own.
- A configured `origin` or `via` names another configured target, from any configuration file
  merged into the effective configuration; one naming no configured target is refused when the
  configuration loads, with the names it could take instead.

Both may be set: `origin: "build"` with `via: "gateway"` makes `build` run SSH through `gateway`,
where `gateway` also has `origin: "build"`.

Registering targets never connects to them; registrations are session-wide.

```javascript
await tool.target_add({name: "database", host: "db.internal", origin: "build",
  transport: {ssh: {key: "~/.ssh/database"}}});
```

## SSH settings

SSH settings live under `transport.ssh`. Every setting is optional. `options` values must be strings;
quote numeric-looking values, for example `ServerAliveInterval: "15"`. Each host is given 30
seconds to connect, after which OpenSSH's own error is the connection error, unless the
target's `options` set `ConnectTimeout`.

| Setting | Meaning |
| --- | --- |
| `user`, `port` | Remote user (default: the origin's username) and port (1–65535, default 22). |
| `key` | A private-key file on the SSH origin, the only key offered. |
| `password` | With `key`, that key's passphrase. Without `key`, the target authenticates with a password and offers no keys. See [passwords](#passwords). |
| `forward_agent` | Forward the agent the connection authenticated with, so remote commands receive `SSH_AUTH_SOCK`. Default `false`. See [agents](#agents). |
| `options` | Extra `ssh_config` options, written verbatim as in an ssh_config file, e.g. `{ ProxyCommand: "sudo -n -u deploy ssh -W %h:%p gateway.internal" }` to reach the host through a gateway with another user's SSH key (commands run without a terminal, so `sudo` must not prompt). Use `%h` for the target host; `%n` is Skyhook's internal host alias. Skyhook-managed settings cannot be overridden. These are rejected: `Host`, `Match`, `Include`, `HostName`, `User`, `Port`, `IdentityFile`, `IdentityAgent`, `AddKeysToAgent`, `BatchMode`, `KbdInteractiveAuthentication`, `ChallengeResponseAuthentication`, `ProxyJump`, `ControlMaster`, `ControlPath`, `ControlPersist`, `ForwardAgent`, `RemoteCommand`, `RequestTTY`, `SessionType`, `CanonicalizeHostname`, `SendEnv`, `SetEnv`, `ForwardX11`, and `ForwardX11Trusted`. `ProxyCommand` cannot be combined with `via`. |

How a target authenticates follows from `key` and `password`:

- `password` without `key`: OpenSSH's password authentication; no keys are offered. A server
  that allows only keyboard-interactive authentication, as some PAM setups do, cannot be reached
  with a password.
- `key`, with an optional `password` for an encrypted key: that key alone.
- neither, including a target with no SSH settings at all: only keys already in the origin's SSH
  agent: the one it inherited or was forwarded, otherwise Skyhook's private agent there if an
  earlier connection started it, or on Windows the agent service. The connection fails if there
  is none; see [agents](#agents).

OpenSSH's default key files (`~/.ssh/id_*`) are never offered.

### Passwords

A `password` takes any [credential form](../configuration/authentication.md#credentials): a
literal, `{env: NAME}`, `{command: "..."}` or `{command: ["program", "arg"]}`, or `{file: path}`:

```yaml
ssh:
  user: deploy
  password:
    command: ["pass", "show", "deploy"]
```

The source is read on the target's origin, the machine that starts its SSH connection: an
environment variable is that machine's, and a command runs, or a file is read, there, exactly as
key paths are interpreted there. It is read again for every connection and never kept.

A password is never included in tool results. A resumed session reads a configured target's
password from the configuration again, and warns of a target the configuration no longer defines
as the session started with it, whose connections fail until it does. A target added with
`target_add` is saved in the session log with its password as given, so a literal one is stored
there in plain text; prefer another form.

A connection's password is given once, to OpenSSH on the origin, which never asks about a host
key, so it reaches no server but the target's. A rejected password fails the connection with
OpenSSH's own reason, and any later prompt fails it, reporting the prompt that was answered. A
password that cannot be read fails the connection with an error naming the
target and the source's problem, never the password. On a Windows origin with OpenSSH for
Windows 8.1, a password or passphrase with non-ASCII characters may not be typed reliably.

Tools run as the same user as Skyhook, so they can read a password file or run a password
command too. Keep password files out of workspaces, and give them mode 600: Skyhook refuses a file
other users can read. On a Windows origin, the file's access list must allow only you, SYSTEM and
Administrators, as for an OpenSSH private key there; see
[credentials](../configuration/authentication.md#credentials).

## WinRM targets

A `winrm` target is a Windows host reached through Windows Remote Management.

```yaml
targets:
  winapp:
    host: "winapp.corp.example"
    workspace: 'C:\deploy'
    transport:
      winrm:
        user: "deploy@CORP.EXAMPLE"
        password: {command: ["pass", "show", "winapp"]}
        auth: "kerberos"

  lab:
    host: "10.0.0.20"
    origin: "build"
    transport:
      winrm:
        user: 'LAB\operator'
        password: {env: "LAB_PASSWORD"}
        tls: {fingerprint: "fb11438a193e3f75d8a64704f74e9554d3774fb17dcf44d868d320ae6ca5fb25"}
```

| Setting | Meaning |
| --- | --- |
| `user` | The account to log on as: `user`, `DOMAIN\user` or `user@REALM`. Required. |
| `password` | Its password, in any [credential form](../configuration/authentication.md#credentials), read on the target's origin at each connection as an SSH [password](#passwords) is. Required for every `auth`. |
| `auth` | `negotiate` (NTLM), `basic` (the user and password with every request), or `kerberos` (a ticket for `HTTP/<host>` from the user's realm). Default `negotiate`. |
| `tls` | `system` (HTTPS, trusting the Mozilla roots and the origin's certificate store), `{ca: path}` (HTTPS, trusting the Mozilla roots and the PEM certificates in that file on the origin; `~/` is the home directory there), `{fingerprint: hex}` (HTTPS, trusting exactly the certificate whose SHA-256 fingerprint this is, in 64 hexadecimal digits, colons between pairs allowed), `insecure` (HTTPS, accepting any certificate, so it does not authenticate the server), or `off` (plain HTTP). Default `system`. |
| `port` | The listener's port; 5986 with TLS and 5985 with `tls: off` by default. |
| `kdc` | With `kerberos`, the KDC as `host` or `host:port` (port 88 by default). Without it, the KDC is the one the origin's Kerberos configuration names, else the one the realm's DNS SRV records (`_kerberos._tcp.<REALM>`, then `_kerberos._udp.<REALM>`) name, looked up with the origin's resolver. |

`auth` and `tls` are independent. With `tls: off`, `negotiate` and `kerberos` encrypt every
message with the keys their logon derived; `basic` does not, so a `basic` target with
`tls: off` sends its user and password, and everything it exchanges, unencrypted, and Windows
accepts it only where its WinRM service allows unencrypted traffic. As with literal secrets in
configuration, choosing that is up to you.

Kerberos takes its realm from the user, uppercased, so write the user as `user@REALM` with the
realm's DNS name, and `host` as the name its `HTTP/<host>` service principal is registered
under, normally the host's DNS name rather than an address. Kerberos reads no ticket cache: it
logs on with the password each time.

A certificate that `tls` does not trust fails the connection with the certificate's SHA-256
fingerprint. To pin a self-signed listener, put that fingerprint in
`tls: {fingerprint: ...}`. A rejected credential fails the connection with the
authentication schemes the listener offers.

The account needs permission to use WinRM's command shell. Administrators have it. Other
accounts need **Execute** in the WinRM service's root security descriptor (its `RootSDDL`);
membership of Remote Management Users alone is not enough. To grant that group Execute, add the
ACE `(A;;GX;;;S-1-5-32-580)` to the descriptor, for example in an elevated PowerShell:

```powershell
$sddl = (Get-Item WSMan:\localhost\Service\RootSDDL).Value
Set-Item WSMan:\localhost\Service\RootSDDL -Value ($sddl -replace 'S:', '(A;;GX;;;S-1-5-32-580)S:') -Force
```

`winrm configSDDL default` edits the same descriptor in a dialog.

A WinRM target connects from its `origin`, root by default, which can be any target, including
a Windows or Linux SSH target or another WinRM target; it is never a `via` jump and takes none.
A Windows origin finds a Kerberos KDC with its own DNS client. WinRM delivers output in batches
about half a second apart, so a tool call takes noticeably longer over WinRM than over SSH.

## Windows targets

A target with `os: "windows"` runs Windows 10 1809, Windows Server 2019 or later. Over SSH it
runs OpenSSH Server with its default shell, cmd.exe; over [WinRM](#winrm-targets) its WinRM
service. Skyhook needs Windows PowerShell 5.1 there, which those releases include.

```yaml
targets:
  winbuild:
    os: "windows"
    host: "winbuild.internal"
    workspace: 'C:\src\project'
    transport:
      ssh:
        user: "builder"
```

Paths on a Windows target are Windows paths, such as `C:\src\main.rs`, and tools accept `/` or
`\` between their parts. A relative `workspace` is in the user's profile directory
(`%USERPROFILE%`). An `exec` command given as a string runs in Windows PowerShell 5.1 without a
profile; an argument list runs its program directly.

## Agents

Each origin authenticates with the SSH agent it already has: on root, the one Skyhook inherited in
`SSH_AUTH_SOCK`; on a remote origin, the agent forwarded to it, if its own connection forwards one.
Only an origin without one gets a private agent that Skyhook starts there when a connection needs
it, and keys from `key` targets are loaded into that private agent as they are used. An inherited
or forwarded agent is never modified: a key is offered directly and not added to it. On root,
`exec` receives the private agent's `SSH_AUTH_SOCK` only when Skyhook inherited none. Private
agent state is discarded when its origin's session or connection ends and is not restored when
resuming a session.

A Windows origin uses the agent in its `SSH_AUTH_SOCK`, or else the OpenSSH Authentication Agent
service while it runs, and never starts a private agent there: that service keeps the keys added to
it across logons, so Skyhook adds none. Without either, a target offering only agent keys, or one
that sets `forward_agent`, fails to connect from that origin, while `key` and `password` targets
connect as usual. A Windows origin connects with OpenSSH's `ssh.exe`, which must be on its search
path.

The agent service does not sign for a logon made with an SSH key rather than a password. From a
Windows origin reached that way, targets offering only agent keys fail while the service runs,
and so does a `key` target whose key the service also holds.

A connection forwards its agent only when its target sets `forward_agent: true`; remote commands
then receive `SSH_AUTH_SOCK`, so commands such as `git`, `ssh` and `ssh-add` can use the same
identities. Without it remote commands get no agent from Skyhook.

`target_add` requires the `write` capability as well as `targets`. It also requires the `exec`
capability and its approval when it sets `ssh.options`, which can run commands, or a `password`
that is not a literal: an environment variable, a file and a command are all read on the origin,
outside any workspace. Configuration files are trusted like MCP commands.

Host keys are checked against the origin user's `~/.ssh/known_hosts` and the system-wide
`/etc/ssh/ssh_known_hosts`, so record each host's key there before connecting; `options` can
name another file with `UserKnownHostsFile`, or accept unknown keys with
`StrictHostKeyChecking: accept-new`, or any key with `no`. An unknown host key fails the
connection with OpenSSH's reason, such as `Host key verification failed.`:
`StrictHostKeyChecking: ask`, OpenSSH's default, acts as `yes`, and `UpdateHostKeys: ask` as
`no`.

## Using targets

Use `targets` to list each target's host, type, `os`, `via`, `origin` and how it authenticates
without connecting.
Registering a target does not change the agent's location.

Target-aware tools, including `agent`, use these rules:

- Omitting `target` uses the calling agent's target and current workspace.
- Setting `target: "root"` uses the session host and its configured session workspace.
- Setting another named target uses its configured workspace, except that explicitly selecting the
  calling agent's current named target retains that agent's workspace override.

Workspace-inheriting tools such as `write`, `replace`, and `remove` use the calling agent's
target and workspace without accepting a target selector; a `write` source names its own target,
so a file can be copied between machines. Host/session tools such as `jobs`,
`todo`, `wait`, and `targets` still run on the session host. `agent` selects a
child's future execution location; launching the child and running its provider remain host
operations. An explicit `agent.workspace` takes precedence over the selected base; relative
overrides resolve against that base.

Diagnostics identify the operation and its subject, with the execution target when applicable.
A working-directory override is separate from the target's configured workspace: an absolute
`cwd` must exist on the selected machine, not on the session host. A connection, session-storage,
or saved-output inspection failure is not necessarily a failure of the command on the remote
machine.

Prefer these named targets to manually running `ssh`. A tool that needs a route requests
approval before any probe, authentication, or shim deployment. The request shows every target
the connection passes through, origins and jumps first, each with its host and the user it logs
in as. A
[route grant](permissions.md#approval-policy) applies across tools and workspaces, so
reconnecting an unchanged route needs no new approval. Changing a target or any hop in its route
invalidates every grant naming it: an operation still waiting for approval or a connection asks
again about the new definition, and one already running over the old route finishes there, but
any further approval it asks for is refused.

Tools run on a target through Skyhook's shim, a program the first connection installs there as
`~/.cache/skyhook/shims/<sha256>`, or under `$XDG_CACHE_HOME` when that is set (on Windows, `%LOCALAPPDATA%\skyhook\shims\<sha256>.exe`). A
shim that starts removes the others there that nothing has used for a day. A session keeps one
connection to each target, shared by every call there.

JavaScript orchestration and the session log stay on the host; nested tool calls use the
selected target and workspace. For remote execution internals and shim builds, see
[remote transport and shims](../development/remote-transport-and-shims.md).

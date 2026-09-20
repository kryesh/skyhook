# Execution targets

Include `"targets"` in a [mode](permissions.md#modes)'s `capabilities` (or a batch job's
`--capabilities` allowlist) to grant the target capability. It is not enabled by default. Without it, target-management
tools, target arguments, JavaScript target setters, and target prompt guidance are all omitted
from the model-visible surface. The former `targets_enabled` setting is no longer accepted.

Targets are named directly under `[targets.<name>]`, and session tools can upsert targets without
modifying TOML. Skyhook never reads SSH configuration files: a target's host, user, port,
authentication, and options come only from its own definition. Aliases, `ProxyJump`, identities,
and other settings in `~/.ssh/config` or `/etc/ssh/ssh_config` do not apply.

When [user and workspace config are layered](../configuration/overview.md#file-selection-and-layering),
a workspace entry replaces the **whole target with the same name**; it does not inherit user SSH,
authentication, or routing settings. Targets with other names are retained. Repeat any
required settings in the replacement entry.

```toml
[targets.bastion]
type = "ssh"
host = "bastion.example.com"

[targets.bastion.ssh]
user = "gateway"
external_agent = true

[targets.build]
type = "ssh"
host = "build.internal"
workspace = "/srv/project"
via = "bastion"

[targets.build.ssh.auth]
kind = "key"
path = "~/.ssh/build_ed25519"

[targets.database]
type = "ssh"
host = "db.internal"
origin = "build"

[targets.database.ssh]
# Reach the host through a gateway using the deploy user's own SSH key on build.
options = { ProxyCommand = "sudo -n -u deploy ssh -W %h:%p gateway.internal" }
```

Targets have an explicit `type`. The built-in `root` is `local`, meaning the session host—the machine
running the main Skyhook process. It always identifies that machine, including when an agent's
current target is remote. Named target configuration and `target_add` accept only `type = "ssh"`.
Older target configurations and session formats are rejected; update configurations and start a
new session.

## Routing: `origin` and `via`

- `origin` names the target whose Skyhook shim starts the SSH connection; it defaults to `root`.
  Key paths and agents belong to the origin.
- `via` names a target to jump through with native `ProxyJump`, within that same connection. Jump
  hosts need no shim. A `via` target must have the same `origin`, so its key paths are never
  interpreted on another machine. Omit `via`, rather than naming the origin, to connect directly
  from the origin.

Both may be set: `origin = "build"` with `via = "gateway"` makes `build` run SSH through `gateway`,
where `gateway` also has `origin = "build"`.

Registering targets never connects to them; registrations are session-wide.

```javascript
await tool.target_add({name: "database", type: "ssh", host: "db.internal", origin: "build",
  ssh: {auth: {kind: "key", path: "~/.ssh/database"}}});
```

## SSH settings

SSH settings live under `ssh`. Every setting is optional:

| Setting | Meaning |
| --- | --- |
| `user`, `port` | Remote user (default: the origin's username) and port (1–65535, default 22). |
| `auth` | `default` offers OpenSSH's default key files (`~/.ssh/id_*`); `agent` offers only keys already in the agent; `key` with `path` offers one private key on the SSH origin. |
| `external_agent` | Authenticate with, and forward, the agent the origin inherited in `SSH_AUTH_SOCK` instead of a private agent Skyhook runs on the origin. Keys are never added to it. Requires the `ssh_agent` capability; see [agents](#agents). |
| `options` | Extra `ssh_config` options, written verbatim as in an ssh_config file, e.g. `{ ProxyCommand = "sudo -n -u deploy ssh -W %h:%p gateway.internal" }` to reach the host through a gateway with another user's SSH key (commands run without a terminal, so `sudo` must not prompt). Use `%h` for the target host; `%n` is Skyhook's internal host alias. Options cannot replace settings Skyhook writes first. These are rejected: `Host`, `Match`, `Include`, `HostName`, `User`, `Port`, `IdentityFile`, `IdentityAgent`, `AddKeysToAgent`, `BatchMode`, `ProxyJump`, `ControlMaster`, `ControlPath`, `ControlPersist`, `ForwardAgent`, `RemoteCommand`, `RequestTTY`, `SessionType`, `CanonicalizeHostname`, `SendEnv`, `SetEnv`, `ForwardX11`, and `ForwardX11Trusted`. `ProxyCommand` cannot be combined with `via`. |

## Agents

Each origin—root, or a target's shim—runs its own private SSH agent, started when a connection from
that origin first needs it; SSH loads keys into it lazily. An `external_agent` target instead uses
the agent its origin inherited in `SSH_AUTH_SOCK`: on root, the one Skyhook was started with; on a
remote origin, the agent forwarded to that origin's connection. A chain of `external_agent` targets
therefore keeps using the same external agent.

Each connection forwards the agent it authenticated with, so remote Skyhook commands receive it in
`SSH_AUTH_SOCK`, allowing commands such as `git`, `ssh`, and `ssh-add` to use the same identities.
Private agent state is discarded when its origin's session or connection ends and is not restored
when resuming a session.

Targets whose route includes an `external_agent` hop, including configured ones, require the
`ssh_agent` capability. Without it they are invisible: `targets` omits them, selecting them or
naming them in `via` or `origin` fails as an unknown target, `target_add` cannot replace them, and
`target_add` does not offer `ssh.external_agent`. With it, connecting requires approval of that
route (listing its origin and `external_agent` hops); an unchanged route is not asked again. Adding
an `external_agent` target with `target_add` is approved separately from later connecting to it.

`target_add` also requires the `exec` capability and its approval when it sets `ssh.options`,
because options such as `ProxyCommand` can run commands. Configuration files are trusted like MCP
commands.

Skyhook's askpass handler routes passwords, key passphrases, keyboard-interactive challenges, host
confirmations, and agent confirmations to the host UI, including prompts from SSH started on a
remote origin. Interactivity is controlled by the handler, not target configuration.
`skyhook batch` has no authentication prompts; invocations without an interactive terminal
must use it and fail operations that require input; `--approve-all` does not approve
authentication prompts. Secrets are never included in tool results or session logs.

## Using targets

The target list shows each target's host, type, `via`, and `origin`. Agents receive their current
target's name, type, host, `via`, and `origin` alongside their effective workspace in
`skyhook_context`. Registering a target does not change the agent's location.

Target-aware tools, including `agent`, use these rules:

- Omitting `target` uses the calling agent's target and current workspace.
- Setting `target: "root"` uses the session host and its configured session workspace.
- Setting another named target uses its configured workspace, except that explicitly selecting the
  calling agent's current named target retains that agent's workspace override.

Tools without a target selector use the calling agent's target and workspace. An explicit
`agent.workspace` takes precedence over the selected base; relative overrides resolve against that base.

Prefer these named targets to manually running `ssh`. The first tool in a session that needs an SSH
route asks for approval before any probe, authentication, or shim deployment. Approval is shared
across tools and workspace-specific pooled connections; reconnecting an unchanged route does not
prompt again. Changing a target or any hop in its route invalidates the affected approval.

JavaScript remains host-owned while nested targeted tools inherit their caller's location. The
`agent` tool also accepts a named target or `root`.
Agents, providers, approvals, and the session log stay on the host; a shim only runs tools and
processes.

For shim selection and catalog injection, see
[remote transport and shims](../development/remote-transport-and-shims.md).

# Execution targets

Include `"targets"` in a [mode](permissions.md#modes)'s `capabilities` (or a batch job's
`--capabilities` allowlist) to grant the target capability. It is not enabled by default.
Without it, agents cannot use target-management tools or select another target, including from
JavaScript. The `external_agent` example below also requires `ssh_agent`; see [agents](#agents).

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
    type: "ssh"
    host: "bastion.example.com"
    ssh:
      user: "gateway"
      external_agent: true

  build:
    type: "ssh"
    host: "build.internal"
    workspace: "/srv/project"
    via: "bastion"
    ssh:
      auth:
        kind: "key"
        path: "~/.ssh/build_ed25519"

  database:
    type: "ssh"
    host: "db.internal"
    origin: "build"
    ssh:
      # Reach the host through a gateway using the deploy user's own SSH key on build.
      options: { ProxyCommand: "sudo -n -u deploy ssh -W %h:%p gateway.internal" }
```

Targets have an explicit `type`. The built-in `root` is `local`, meaning the session host—the machine
running the main Skyhook process. It always identifies that machine, including when an agent's
current target is remote. Named target configuration and `target_add` accept only `type: "ssh"`.

## Routing: `origin` and `via`

- `origin` names the target that starts the SSH connection; it defaults to `root`.
  Key paths and agents belong to the origin.
- `via` names a target to jump through with native `ProxyJump`, within that same connection. Jump
  hosts need no shim. A `via` target must have the same `origin`, so its key paths are never
  interpreted on another machine. Omit `via`, rather than naming the origin, to connect directly
  from the origin.

Both may be set: `origin: "build"` with `via: "gateway"` makes `build` run SSH through `gateway`,
where `gateway` also has `origin: "build"`.

Registering targets never connects to them; registrations are session-wide.

```javascript
await tool.target_add({name: "database", type: "ssh", host: "db.internal", origin: "build",
  ssh: {auth: {kind: "key", path: "~/.ssh/database"}}});
```

## SSH settings

SSH settings live under `ssh`. Every setting is optional. `options` values must be strings;
quote numeric-looking values, for example `ServerAliveInterval: "15"`.

| Setting | Meaning |
| --- | --- |
| `user`, `port` | Remote user (default: the origin's username) and port (1–65535, default 22). |
| `auth` | `default` offers OpenSSH's default key files (`~/.ssh/id_*`); `agent` offers only keys already in the agent; `key` with `path` offers one private key on the SSH origin. |
| `external_agent` | Authenticate with, and forward, the agent the origin inherited in `SSH_AUTH_SOCK` instead of a private agent Skyhook runs on the origin. Keys are never added to it. Requires the `ssh_agent` capability; see [agents](#agents). |
| `options` | Extra `ssh_config` options, written verbatim as in an ssh_config file, e.g. `{ ProxyCommand: "sudo -n -u deploy ssh -W %h:%p gateway.internal" }` to reach the host through a gateway with another user's SSH key (commands run without a terminal, so `sudo` must not prompt). Use `%h` for the target host; `%n` is Skyhook's internal host alias. Skyhook-managed settings cannot be overridden. These are rejected: `Host`, `Match`, `Include`, `HostName`, `User`, `Port`, `IdentityFile`, `IdentityAgent`, `AddKeysToAgent`, `BatchMode`, `ProxyJump`, `ControlMaster`, `ControlPath`, `ControlPersist`, `ForwardAgent`, `RemoteCommand`, `RequestTTY`, `SessionType`, `CanonicalizeHostname`, `SendEnv`, `SetEnv`, `ForwardX11`, and `ForwardX11Trusted`. `ProxyCommand` cannot be combined with `via`. |

## Agents

By default, Skyhook uses a private SSH agent on each origin for keys loaded during the session.
An `external_agent` target instead uses the agent its origin inherited in `SSH_AUTH_SOCK`: on root,
the one Skyhook was started with; on a remote origin, the agent forwarded to that origin's
connection. A chain of `external_agent` targets therefore keeps using the same external agent.

Each connection forwards the agent it authenticated with, so remote Skyhook commands receive it in
`SSH_AUTH_SOCK`, allowing commands such as `git`, `ssh`, and `ssh-add` to use the same identities.
Private agent state is discarded when its origin's session or connection ends and is not restored
when resuming a session.

Targets whose route includes an `external_agent` hop, including configured ones, require the
`ssh_agent` capability. Without it they are invisible: `targets` omits them, selecting them or
naming them in `via` or `origin` fails as an unknown target, `target_add` cannot replace them, and
`target_add` does not offer `ssh.external_agent`. With it, connecting requires approval of that
route (listing its origin and `external_agent` hops). Adding an `external_agent` target with
`target_add` is approved separately from later connecting to it.

`target_add` requires the `write` capability as well as `targets`. It also requires the `exec`
capability and its approval when it sets `ssh.options`,
because options such as `ProxyCommand` can run commands. Configuration files are trusted like MCP
commands.

Passwords, key passphrases, keyboard-interactive challenges, host confirmations, and agent
confirmations appear in Skyhook's interface, including prompts from SSH started on a remote
origin. Target configuration cannot enable prompts in a noninteractive session.
`skyhook batch` has no authentication prompts; invocations without an interactive terminal
must use it and fail operations that require input; `--approve-all` does not approve
authentication prompts. Secrets are never included in tool results or session logs.

## Using targets

Use `targets` to list each target's host, type, `via`, and `origin` without connecting.
Registering a target does not change the agent's location.

Target-aware tools, including `agent`, use these rules:

- Omitting `target` uses the calling agent's target and current workspace.
- Setting `target: "root"` uses the session host and its configured session workspace.
- Setting another named target uses its configured workspace, except that explicitly selecting the
  calling agent's current named target retains that agent's workspace override.

Workspace-inheriting tools such as `write`, `replace`, and `remove` use the calling agent's
target and workspace without accepting a target selector. Host/session tools such as `jobs`,
`job_output`, `todo`, `wait`, and `targets` still run on the session host. `agent` selects a
child's future execution location; launching the child and running its provider remain host
operations. An explicit `agent.workspace` takes precedence over the selected base; relative
overrides resolve against that base.

Diagnostics identify the operation and its subject, with the execution target when applicable.
A working-directory override is separate from the target's configured workspace: an absolute
`cwd` must exist on the selected machine, not on the session host. A connection, session-storage,
or saved-output inspection failure is not necessarily a failure of the command on the remote
machine.

Prefer these named targets to manually running `ssh`. A tool that needs an SSH route requests
approval before any probe, authentication, or shim deployment. A saved
[route grant](permissions.md#approval-policy) applies across tools and workspaces, so
reconnecting an unchanged route needs no new approval. Changing a target or any hop in its route
invalidates the affected grant.

JavaScript orchestration and the session log stay on the host; nested tool calls use the
selected target and workspace. For remote execution internals and shim builds, see
[remote transport and shims](../development/remote-transport-and-shims.md).

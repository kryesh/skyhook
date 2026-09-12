# Execution targets

Include `"targets"` in the top-level `capabilities` array (or CLI `--capabilities` allowlist) to
grant the session's target capability. It is not enabled by default. Without it, target-management
tools, target arguments, JavaScript target setters, and target prompt guidance are all omitted
from the model-visible surface. The former `targets_enabled` setting is no longer accepted.

Targets are named directly under `[targets.<name>]`. Importing concrete aliases from the user's
SSH configuration is disabled by default. Session tools can upsert targets without modifying TOML.

When [user and workspace config are layered](../configuration/overview.md#file-selection-and-layering),
a workspace entry replaces the **whole target with the same name**; it does not inherit user SSH,
authentication, origin, or routing settings. Targets with other names are retained. Repeat any
required settings in the replacement entry.

```toml
[targets]
import_ssh_config = false

[targets.bastion]
type = "ssh"
host = "bastion.example.com"

[targets.bastion.ssh]
user = "gateway"

[targets.build]
type = "ssh"
host = "build.internal"
workspace = "/srv/project"
via = "bastion"

[targets.build.ssh.auth]
kind = "key"
path = "~/.ssh/build_ed25519"
```

Targets have an explicit `type`. The built-in `root` is `local`, meaning the session host—the machine
running the main Skyhook process. It always identifies that machine, including when an agent's
current target is remote. Named target configuration and `target_add` accept only `type = "ssh"`.
SSH-specific settings live under `ssh`, including authentication kinds `openssh` (default),
`agent` (the session's managed agent), and `key` (an explicit path). Older target configurations and
session formats are rejected; update configurations and start a new session.

The target list shows each target's effective destination hostname/IP, original SSH alias, type,
origin, and `via`. SSH imports resolve aliases with OpenSSH and translate `ProxyJump` into named
`via` chains, creating stable names for unnamed or route-specific hops. Opaque `ProxyCommand`
settings remain SSH-specific. Listing targets does not connect to their destinations.

Agents receive their current target's name, type, resolved hostname, origin, and `via` alongside
their effective workspace in `skyhook_context`. Registering a target does not change the agent's
location.

`via` describes reachability; `origin` identifies the machine where connection configuration and
outbound SSH processes belong. Local config/imports originate on `root`. `target_add` defaults its
origin to the caller's target and accepts an explicit `origin` override:

```javascript
await tool.target_add({name: "database", type: "ssh", host: "db.internal",
  origin: "build", ssh: {auth: {kind: "key", path: "~/.ssh/database"}}});
```

The resulting target automatically uses `via: "build"`, extended by any jump chain in build's SSH
configuration. Registration resolves configuration on the origin, which may require connecting to
it, but never connects to the new destination or decrypts its keys. Registrations are session-wide.
A root-origin connection via build uses native SSH forwarding; a build-origin connection runs SSH
under build's shim. Native forwarding alone does not require a shim on jump hosts. Native hops in
a connection segment must share its configuration origin; use an explicit remote origin to start
a shim-owned continuation. This prevents remote credential paths being interpreted on root.

One private, session-owned SSH agent runs on root. SSH loads keys lazily from their configuration
origin into this central agent. Remote Skyhook commands receive OpenSSH’s private forwarded socket in
`SSH_AUTH_SOCK`, allowing commands such as `git`, `ssh`, and `ssh-add` to use the same identities.
Skyhook does not inherit an existing user agent. Agent state is discarded at session shutdown and
is not restored when resuming a session.

Skyhook's askpass handler routes passwords, key passphrases, keyboard-interactive challenges, host
confirmations, and agent confirmations to the host UI. Interactivity is controlled by the handler,
not target configuration. `--non-interactive` disables authentication prompts; invocations without an interactive
terminal must use that mode and fail operations that require input; `--approve-all` does not approve authentication prompts. Secrets are never
included in tool results or session logs.

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
prompt again. Changing a target or any hop in its `via` route invalidates the affected approval.

JavaScript remains host-owned while nested targeted tools inherit their caller's location. The
`agent` tool also accepts a named target or `root`.
Remote agent state machines, providers, approvals, message history, and canonical session logging
remain host-owned. The shim retains only live remote tool/process execution state and unclaimed
background-job results; its worker store is ephemeral.

For shim selection and catalog injection, see
[remote transport and shims](../development/remote-transport-and-shims.md).

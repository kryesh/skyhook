# Configuration overview

Skyhook uses TOML configuration. The files are not rewritten when the terminal remembers a
model.

## File selection and layering

With **`--config path.toml`**, Skyhook loads **only that file**. It does not search for or merge
user or workspace configuration, even if the explicit file fails to load.

Without `--config`, Skyhook:

1. Tries `$XDG_CONFIG_HOME/skyhook/config.toml`, when that environment variable is set.
2. On **any read, TOML parse, or structural error**, tries `$HOME/.config/skyhook/config.toml`.
   At most one user file is selected; the two user files are never merged.
3. Overlays `<resolved workspace>/.skyhook/config.toml` if it exists. `--workspace PATH`
   selects the workspace; otherwise it is the invocation directory. No ancestor workspace
   configurations are loaded. An unreadable or invalid workspace file is an error.

Missing optional user files are fine: a complete workspace-only configuration can be used.
A broken user file is not silently treated as missing if no fallback succeeds. The effective
configuration must be valid; defaults are applied **after merging**.

Tables merge recursively; arrays and scalar values replace the earlier value. The exception is
**named entries under `targets`**: a workspace `[targets.build]` replaces the *whole* user
`[targets.build]` entry, including its SSH, authentication, workspace, and routing settings.
It does not inherit omitted fields. Other named targets remain available.

### Example: user models, workspace targets

Keep provider/model definitions in the selected user `config.toml`:

```toml
[providers.openai]
kind = "openai"
base_url = "https://api.openai.com/v1"
api = "responses"
api_key_env = "OPENAI_API_KEY"

[models.default]
provider = "openai"
model = "gpt-5.6"
max_context = 1050000
max_output = 128000

[targets.bastion]
type = "ssh"
host = "bastion.example.com"

[targets.build]
type = "ssh"
host = "old-build.internal"
via = "bastion"

[targets.build.ssh.auth]
kind = "key"
path = "~/.ssh/old-build"
```

Then use this `<workspace>/.skyhook/config.toml`:

```toml
[modes.general]
capabilities = ["read", "write", "exec", "targets"]

[targets.build]
type = "ssh"
host = "new-build.example.com"
workspace = "/srv/project"
```

The effective configuration retains `providers.openai`, `models.default`, and `targets.bastion`.
A named mode or target is replaced whole, not merged: `modes.general` grants exactly the listed
capabilities, and `targets.build` uses the new host and workspace,
with default authentication/routing behavior: neither the old key nor `via = "bastion"` survives.
If a route or key is needed, repeat it explicitly in the workspace entry.

**Trust boundary:** workspace configuration is full configuration, not a restricted project hint.
It can change approvals, modes, provider credential commands, and MCP server startup.
Review it before starting Skyhook in an untrusted checkout. This repository currently ignores
`.skyhook/`, so creating `.skyhook/config.toml` does **not** automatically make it versioned.

## Paths and environment

Relative MCP `cwd` values resolve against the directory of the **source file defining that
value**. A user-defined `cwd` keeps its user-file base when other MCP fields are overlaid;
a workspace-defined `cwd` uses the workspace config's directory (`<workspace>/.skyhook`),
not the workspace root.

The CLI always stores and discovers sessions in `<workspace>/.skyhook/sessions`, including
headless execution and resume. It does not search other workspaces or honor `session_root` overrides.
For library users, `session_root` can still override storage; relative values use the process working
directory, not the config file's directory. Remote target paths retain their existing remote
semantics. The workspace is a base directory, not filesystem isolation.

The CLI still loads **`.env` from the invocation directory**, with existing process environment
variables taking precedence. Neither `--workspace` nor `--config` changes that location.
See [authentication](authentication.md#api-keys-and-environment-files).

## Inspect without starting a session

```sh
skyhook dump --workspace /path/to/project         # Same as dump config
skyhook dump config --workspace /path/to/project --approve-all
skyhook dump config --config ./standalone.toml    # Explicit file only
```

These commands write resolved effective TOML to stdout and source paths/diagnostics to stderr.
They apply defaults and applicable CLI policy overrides without requiring API credentials,
starting MCP servers, or making network requests. Exit status is zero for a valid dump and
nonzero if valid configuration cannot be produced. See [dump](../reference/cli.md#dump)
for selectors and incompatible options.

## Configuration areas

- [Providers and models](providers-and-models.md): named API connections, full model IDs,
  context/output limits, reasoning replay, and connection recovery.
- [Authentication](authentication.md): environment variables, secret-manager commands,
  `.env`, and Skyhook-owned Codex login.
- [Permissions and capabilities](../guide/permissions.md): `approve_all`, modes, and their exact
  capability allowlists.
- [Execution targets](../guide/execution-targets.md): SSH destinations, origins, routing,
  and optional SSH-config import.
- [MCP servers](mcp.md): trusted local server startup and imported tools.

## Complete example

This is included from the repository's single maintained
[`config.example.toml`](https://github.com/kryesh/skyhook/blob/main/config.example.toml).
It demonstrates multiple providers; remove unused profiles or supply their credentials before
running a session. For a minimal configuration, follow [your first session](../getting-started/first-session.md).

```toml
{{#include ../../../config.example.toml}}
```

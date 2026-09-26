# Configuration overview

Skyhook uses YAML configuration. The files are not rewritten when the terminal remembers a
model.

## File selection and layering

With **`--config path.yaml`**, Skyhook loads **only that file**. It does not search for or merge
user or workspace configuration, even if the explicit file fails to load. The contents are always
parsed as YAML, regardless of the filename suffix.

Without `--config`, Skyhook:

1. Tries `$XDG_CONFIG_HOME/skyhook/config.yaml`, when that environment variable is set.
2. On **any read, YAML parse, or structural error**, tries `$HOME/.config/skyhook/config.yaml`.
   At most one user file is selected; the two user files are never merged.
3. Overlays `<resolved workspace>/.skyhook/config.yaml` if it exists. `--workspace PATH`
   selects the workspace; otherwise it is the invocation directory. No ancestor workspace
   configurations are loaded. An unreadable or invalid workspace file is an error.

Automatic discovery uses only `config.yaml`, not `config.yml` or `config.toml`; there is no TOML
fallback.

Missing optional user files are fine: a complete workspace-only configuration can be used.
A broken user file is not silently treated as missing if no fallback succeeds. The effective
configuration must be valid; defaults are applied **after merging**.

Mappings merge recursively; sequences and scalar values replace the earlier value. The exception is
**named entries under `targets` and `modes`**: a workspace entry replaces the *whole* user
entry of the same name, without inheriting omitted fields. For example, `targets.build`
replaces that target's SSH, authentication, workspace, and routing settings; `modes.general`
replaces that mode's capabilities, instructions, and hint. Other named entries remain available.
A provider entry that changes `dialect` likewise replaces the earlier provider's settings, which
belong to the other dialect, while the models declared under it still merge by name. Within a
provider, a later layer replaces `api_key` and each `headers` value whole, and likewise each
convention choice (`cache_key`, `output_limit`, `reasoning_effort`, `reasoning_replay`,
`tool_stream`, `user_id`) at entry level or under a model's `overrides`: a workspace
`api_key: {command: ...}` or `cache_key: {body: ...}` takes the place of a user `{env: ...}` or
`{header: ...}` rather than combining with it. An explicit `null` replaces the earlier value: it
clears an optional field, but is not a deletion operator for map entries. Required fields and named
entries must still have valid values.

### Example: user models, workspace targets

Keep providers, with their models, in the selected user `config.yaml`:

```yaml
providers:
  openai:
    codec: "responses"
    dialect: "openai"
    base_url: "https://api.openai.com/v1"
    api_key:
      env: "OPENAI_API_KEY"
    models:
      default:
        model: "gpt-5.6"
        max_context: 1050000
        max_output: 128000

targets:
  bastion:
    type: "ssh"
    host: "bastion.example.com"

  build:
    type: "ssh"
    host: "old-build.internal"
    via: "bastion"
    ssh:
      auth:
        kind: "key"
        path: "~/.ssh/old-build"
```

Then use this `<workspace>/.skyhook/config.yaml`:

```yaml
modes:
  general:
    capabilities: ["read", "write", "exec", "targets"]

targets:
  build:
    type: "ssh"
    host: "new-build.example.com"
    workspace: "/srv/project"
```

The effective configuration retains `providers.openai`, `models.default`, and `targets.bastion`.
A named mode or target is replaced whole, not merged: `modes.general` grants exactly the listed
capabilities, and `targets.build` uses the new host and workspace,
with default authentication/routing behavior: neither the old key nor `via: "bastion"` survives.
If a route or key is needed, repeat it explicitly in the workspace entry.

**Trust boundary:** workspace configuration is full configuration, not a restricted project hint.
It can change approvals, modes, provider credential commands, and MCP server startup.
Review it before starting Skyhook in an untrusted checkout.

## YAML syntax and strings

Each file must contain a single YAML document with a **mapping (object) at its root**. Empty or
comment-only documents, null roots, and sequence or scalar roots are rejected. An empty mapping
(`{}`) is a valid no-op workspace overlay, but an empty effective configuration is invalid.

Values must be JSON-compatible: mappings with string keys, sequences, strings, booleans, finite
numbers, and null. Duplicate keys, non-string keys, custom tags, non-finite numbers, multiple
documents, and YAML `<<` merge keys are rejected. Anchors and aliases are supported with bounded
expansion. YAML merge keys are unrelated to Skyhook's file layering.

Use indentation for nested settings, not dotted table headers. Quote values that must stay strings,
especially numeric-looking SSH options and environment values: `ServerAliveInterval: "15"` and
`WORKERS: "4"`, not unquoted numbers. Actual numeric settings such as `max_context` remain numbers.

For multiline instructions or shell-command strings, a literal block scalar preserves line breaks:

```yaml
modes:
  readonly:
    capabilities: ["read", "network"]
    instructions: |-
      Investigate and report.
      Do not change anything.
```

`|-` omits the final newline; use `|` if the string should end with a newline. Folded blocks
(`>` or `>-`) turn ordinary line breaks into spaces, so use them only when that is intended.
MCP `start_command` remains a sequence of argument strings, not a shell-command block.

## Migrating from TOML

Manually convert the contents of each old `config.toml` to YAML, then rename it to `config.yaml`.
Replace table headers with nested mappings and `key = value` assignments with `key: value`;
keep the same configuration keys, values, and model declaration order. Check quoting and multiline
string whitespace rather than only changing punctuation. Update explicit `--config` paths too.
Renaming alone does not convert TOML, and Skyhook does not convert files automatically or fall back
to the old format. Use `skyhook dump config` to validate and inspect the effective YAML.

## Paths and environment

Relative MCP `cwd` values resolve against the directory of the **source file defining that
value**. A user-defined `cwd` keeps its user-file base when other MCP fields are overlaid;
a workspace-defined `cwd` uses the workspace config's directory (`<workspace>/.skyhook`),
not the workspace root.

The CLI always stores and discovers sessions in `<workspace>/.skyhook/sessions`, including
headless execution and resume. It does not search other workspaces or honor `session_root` overrides.
Remote paths resolve on the selected [execution target](../guide/execution-targets.md).
The workspace is a base directory, not filesystem isolation.

The CLI loads **`.env` from the invocation directory**, with existing process environment
variables taking precedence. Neither `--workspace` nor `--config` changes that location.
See [authentication](authentication.md#api-keys-and-environment-files).

## Inspect without starting a session

```sh
skyhook dump --workspace /path/to/project         # Same as dump config
skyhook dump config --workspace /path/to/project --approve-all
skyhook dump config --config ./standalone.yaml    # Explicit file only
```

These commands write resolved effective YAML to stdout and source paths/diagnostics to stderr.
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
  and authentication.
- [MCP servers](mcp.md): trusted local server startup and imported tools.

## Complete example

This is included from the repository's single maintained
[`config.example.yaml`](https://github.com/kryesh/skyhook/blob/main/config.example.yaml).
It demonstrates multiple providers; remove unused providers and models or supply their credentials
before running a session. For a minimal configuration, follow
[your first session](../getting-started/first-session.md).

```yaml
{{#include ../../../config.example.yaml}}
```

# Your first session

After [installing Skyhook](installation.md), create a configuration directory. The default
location is `$XDG_CONFIG_HOME/skyhook` or `~/.config/skyhook`:

```sh
config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/skyhook"
mkdir -p "$config_dir"
```

Create `config.yaml` in that directory with a provider and model you can access. For example:

```yaml
providers:
  openai:
    kind: "openai"
    base_url: "https://api.openai.com/v1"
    api: "responses"
    api_key_env: "OPENAI_API_KEY"

models:
  default:
    provider: "openai"
    model: "gpt-5.6"
    max_context: 1050000
    max_output: 128000
```

Use a model identifier supported by your account and set its token limits explicitly; Skyhook
does not detect them automatically. You can choose smaller budgets. For model limits, other
providers, a keyless local server, or Codex OAuth, see
[providers and models](../configuration/providers-and-models.md) and
[authentication](../configuration/authentication.md).

From a source checkout, you can instead copy the comprehensive example and edit it before running:

```sh
cp config.example.yaml "$config_dir/config.yaml"
```

The [full example](../configuration/overview.md#complete-example) configures several providers;
remove unused providers and models or supply their required credentials.

Start from the project directory you want to work on:

```sh
export OPENAI_API_KEY=...
skyhook --prompt "inspect this repository"
```

Skyhook opens a full-screen terminal interface. `--prompt` submits an initial message;
`--script workflow.js` starts a JavaScript workflow in the same interface. Both remain open
for inspection and follow-up input after the work finishes. These interface modes require an
interactive terminal. Use `skyhook batch` with `-p/--prompt` or `-s/--script` for headless execution
with redirected input/output support and automatic exit; see [Headless execution](../guide/headless.md).

An unused startup draft creates no saved session. The first sent message or explicitly run
script creates the session. Use `--workspace PATH` to select another workspace. Normally,
`<workspace>/.skyhook/config.yaml` overlays the selected user configuration. Use
`--config path.yaml` to load **only** that file instead, with no user/workspace config merging.
See [configuration layering](../configuration/overview.md#file-selection-and-layering), including
its workspace trust boundary.

To inspect configuration or available skills without a session or API credentials:

```sh
skyhook dump config  # Effective YAML on stdout; diagnostics/source paths on stderr.
skyhook dump skills  # Winning skills, frontmatter, and asset tree.
```

Both return nonzero on errors; skills can still list valid entries when other entries fail.
See [dump](../reference/cli.md#dump) for compatible options. After a session,
`skyhook stats SESSION_ID` reports its token usage, model requests, delegation, and tool calls
(see [stats](../reference/cli.md#stats)).

Actions with effects outside the workspace require approval. See
[permissions and capabilities](../guide/permissions.md) before using `--approve-all`; it bypasses
tool approvals, not authentication prompts.

Next, learn the [terminal interface](../guide/terminal-interface.md),
[session and context behavior](../guide/sessions-and-context.md), or
[headless execution](../guide/headless.md). All command-line options are summarized in the
[CLI reference](../reference/cli.md).

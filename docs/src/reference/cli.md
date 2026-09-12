# Command-line reference

```text
skyhook [OPTIONS]
skyhook auth <login|status|logout> [codex]
```

Run `skyhook --help`, `skyhook --version`, or `skyhook auth --help` for the installed
binary's command help. Session execution requires an interactive terminal unless
`--non-interactive` is supplied. Dump modes do not need a terminal.

## Session options

| Option | Meaning |
| --- | --- |
| `--config PATH` | Use only this TOML file; disable user/workspace config searching and merging. |
| `--workspace PATH` | Set the workspace base directory (default `.`). |
| `--resume SESSION_ID` | Reopen a saved session. |
| `-m, --model PROFILE` | Choose a configured model profile for a new session. Resumed sessions retain their recorded model. |
| `-p, --prompt TEXT` | Submit an initial user message. |
| `-s, --script PATH` | Run a JavaScript workflow. Mutually exclusive with `--prompt`. |
| `--image PATH` | Attach an image to the initial prompt; repeat for multiple images. Requires `--prompt`, conflicts with `--script`. |
| `--non-interactive` | Run headlessly with exactly one prompt or script, then exit. |
| `--approve-all` | Bypass tool approval prompts, not capability gates or authentication prompts. |
| `--capabilities LIST` | Replace the configured policy allowlist with a comma-separated list. `--capabilities=` grants no policy capabilities. |
| `-h, --help` | Print command help. |
| `-V, --version` | Print the version. |

Without `--non-interactive`, an initial prompt or script runs in the terminal interface,
which remains open afterward. Startup without initial input opens an empty draft; no session
is created until a message or script is submitted.

Headless stdout contains only the session ID and newline, flushed before execution; results
remain in session logs. See the [headless guide](../guide/headless.md) for completion, errors,
shutdown, signals, and interaction restrictions. `--non-interactive` is not an authentication
subcommand option and cannot be combined with `auth`.

Policy capability names are `read`, `write`, `exec`, `network`, `targets`, `agents`, and `mcp`.
The runtime supplies `interactive`; it is invalid in the CLI allowlist. See
[permissions](../guide/permissions.md) for resolution and limitations.

## Dump modes

`--dump` defaults to `--dump config`. The only selectors are **`config`** and **`skills`**;
unknown selectors (including `agents`) are errors. `--workspace PATH` applies to both modes.

```sh
skyhook --dump                        # Resolved effective TOML
skyhook --dump config > effective.toml
skyhook --workspace /path/to/project --dump config
skyhook --config ./standalone.toml --dump config --capabilities=read --approve-all
skyhook --workspace /path/to/project --dump skills
```

Both modes are inspection-only: no terminal UI, harness, session creation, provider credential
commands, MCP startup, or network access. API credentials are not required. They do not execute
skill assets.

### Configuration dump

The config dump writes the resolved effective configuration as TOML to **stdout**. It follows
normal [file selection and layering](../configuration/overview.md#file-selection-and-layering),
including explicit-file-only behavior for `--config`, and includes defaults plus any
`--capabilities` and `--approve-all` overrides. Source paths and diagnostics go to **stderr**,
so redirected stdout remains TOML. It exits with status **0** when a valid config can be produced,
and **nonzero** otherwise; it does not require provider credentials to validate configuration.

The dump includes literal values stored in configuration, such as MCP environment entries;
treat its output as potentially sensitive. Environment-backed API keys are not expanded and
credential commands are not executed. Diagnostic control characters are escaped for safe
terminal display.

### Skills dump

The skills dump uses the same **HostSkills discovery** as the harness, independently of the main
model configuration. It works without a model config or API credentials. The output is a tree of
winning skills (after name precedence), including each source path, effective summary, complete
YAML frontmatter, and nested assets in sorted order. Symlinks are marked and not followed;
asset contents are not executed.

Skill parse and asset traversal errors go to **stderr**, while valid skills are still listed on
**stdout**. Exit status is **0** when discovery and asset inspection succeed, or **nonzero** if
errors were reported, even if stdout contains some valid skills. This does not change skill
search locations: `~/.agents/skills` plus `.agents/skills` along workspace ancestry. See
[instructions and skills](../guide/instructions-and-skills.md).

### Incompatible options

Both dump modes reject `--prompt`, `--script`, `--resume`, `--image`, `--non-interactive`,
`--model`, and the `auth` subcommand. `--dump skills` additionally rejects `--config`,
`--capabilities`, and `--approve-all` as irrelevant. Invalid selectors and conflicting options
exit nonzero rather than starting a session.

## Authentication commands

```sh
skyhook auth login
skyhook auth login --headless
skyhook auth status
skyhook auth logout
```

The optional provider argument currently accepts only `codex`, which is the default.
`auth login --headless` selects device authorization rather than a browser; it is separate
from session `--non-interactive`. Login does not require model configuration.
See [authentication](../configuration/authentication.md) for credential ownership and storage.

## Related settings

The [configuration overview](../configuration/overview.md) describes configuration paths
and the full example. [Authentication](../configuration/authentication.md#api-keys-and-environment-files)
documents invocation-directory `.env` loading. [Terminal interface](../guide/terminal-interface.md)
documents slash commands, keybindings, model selection, and saved UI state.

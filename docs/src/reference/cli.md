# Command-line reference

```text
skyhook [OPTIONS]
skyhook batch [OPTIONS] <--prompt TEXT|--script PATH>
skyhook auth <login|status|logout> [codex]
skyhook dump [config|skills] [OPTIONS]
skyhook stats <SESSION_ID|--list> [OPTIONS]
```

Run `skyhook --help`, `skyhook --version`, or `skyhook <subcommand> --help` for the installed
binary's command help. `skyhook` itself requires an interactive terminal; `batch`, `dump` and
`stats` do not.

## Session options

| Option | Meaning |
| --- | --- |
| `-c, --config PATH` | Use only this file (YAML regardless of suffix); disable user/workspace config searching and merging. |
| `-w, --workspace PATH` | Set the workspace base directory (default `.`). |
| `--resume SESSION_ID` | Reopen a saved session. |
| `-m, --model PROVIDER/MODEL` | Choose a configured model for a new session, named by its provider and model keys. Resumed sessions retain their recorded model. |
| `-p, --prompt TEXT` | Submit an initial user message. |
| `-s, --script PATH` | Run a JavaScript workflow. Mutually exclusive with `--prompt`. |
| `--image PATH` | Attach an image to the initial prompt; repeat for multiple images. Requires `--prompt`, conflicts with `--script`. |
| `--mode NAME` | Start the new-session prompt in this configured [mode](../guide/permissions.md#modes) instead of the default. With `--resume`, the next message switches the session to it. |
| `-a, --approve-all` | Bypass tool approval prompts, not capability gates or authentication prompts. |
| `-h, --help` | Print command help. |
| `-V, --version` | Print the version. |

An initial prompt or script runs in the terminal interface, which remains open afterward.
Startup without initial input opens an empty draft; no session is created until a message or
script is submitted. These options belong to session execution only: a subcommand accepts
just its own options, after its name.

## Batch

`skyhook batch` runs exactly one prompt or script without a terminal, then exits. It takes the
session options above except that `--prompt` or `--script` is required, plus:

| Option | Meaning |
| --- | --- |
| `--mode NAME` | Run in this configured mode instead of the default. With `--resume`, the prompt switches the session to it; a script has no prompt, so that combination is an error. |
| `--capabilities LIST` | Run with exactly this comma-separated policy allowlist instead of a mode. `--capabilities=` grants none. Conflicts with `--mode`. |

Stdout contains only the session ID and newline, flushed before execution; results remain in
session logs. See the [headless guide](../guide/headless.md) for completion, errors, shutdown,
signals, and interaction restrictions.

Policy capability names are `read`, `write`, `exec`, `network`, `targets`, `ssh_agent`,
`agents`, and `mcp`. The terminal supplies `interactive`; a batch job never has it. See
[permissions](../guide/permissions.md) for resolution and limitations.

## Dump

`skyhook dump` defaults to `skyhook dump config`. The only selectors are **`config`** and
**`skills`**; unknown selectors are errors.

| Option | Meaning |
| --- | --- |
| `-w, --workspace PATH` | Workspace whose configuration or skills to inspect (default `.`). |
| `-c, --config PATH` | `config` only: use only this file (YAML regardless of suffix), with no user/workspace merging. |
| `-a, --approve-all` | `config` only: apply the approval override. |

```sh
skyhook dump                          # Resolved effective YAML
skyhook dump config > effective.yaml
skyhook dump config --workspace /path/to/project
skyhook dump config --config ./standalone.yaml --approve-all
skyhook dump skills --workspace /path/to/project
```

Both modes are inspection-only: they do not start a session, run provider credential commands,
connect to MCP servers, access the network, or execute skill assets. API credentials are not
required. `dump skills` rejects `--config` and `--approve-all` as irrelevant.

### Configuration dump

The config dump writes the resolved effective configuration as YAML to **stdout**. It follows
normal [file selection and layering](../configuration/overview.md#file-selection-and-layering),
including explicit-file-only behavior for `--config`, and includes defaults (such as the
built-in `general` mode) plus any `--approve-all` override. Source paths and diagnostics go to **stderr**,
so redirected stdout remains YAML. It exits with status **0** when a valid config can be produced,
and **nonzero** otherwise; it does not require provider credentials to validate configuration.

The dump includes literal values stored in configuration, such as a literal `api_key` or header
value and MCP environment entries; treat its output as potentially sensitive. Environment-backed
values are not expanded and credential commands are not executed. Diagnostic control characters
are escaped for safe terminal display.

### Skills dump

The skills dump uses the same skill discovery as a session, independently of model configuration.
It works without a model config or API credentials. The output is a tree of
winning skills (after name precedence), including each source path, effective summary, complete
YAML frontmatter, and nested assets in sorted order. Symlinks are marked and not followed;
asset contents are not executed.

Skill parse and asset traversal errors go to **stderr**, while valid skills are still listed on
**stdout**. Exit status is **0** when discovery and asset inspection succeed, or **nonzero** if
errors were reported, even if stdout contains some valid skills. This does not change skill
search locations: `~/.agents/skills` plus `.agents/skills` along workspace ancestry. See
[instructions and skills](../guide/instructions-and-skills.md).

## Stats

`skyhook stats SESSION_ID` reads a saved session and reports where its tokens went,
how its model requests ended, what each agent delegated, and which tools it called. Like
`--resume`, it finds the session in the selected workspace's history
(`<workspace>/.skyhook/sessions`). It needs no configuration, credentials, or terminal, and it
can read a session another process still has open.

| Option | Meaning |
| --- | --- |
| `-l, --list` | List the workspace's sessions, newest first, instead of reporting one; takes no `--format`. |
| `-w, --workspace PATH` | Workspace whose session history holds the session (default `.`). |
| `-f, --format markdown\|tree\|json` | Machine-readable output; omitted, the tables are rendered for reading. |

```sh
skyhook stats 94e934f0ee9c1e314c27d91977919462
skyhook stats 94e934f0ee9c1e314c27d91977919462 --format tree
skyhook stats 94e934f0ee9c1e314c27d91977919462 --format json | jq '.agents[].tools'
skyhook stats --list
```

Without `--format`, the report is a session header (initial prompt, start time, duration, request and
token totals) followed by aligned tables: one row per agent (path, model, completed/requested
model calls, tool calls, input, cached, cache-written, and output tokens, duration) with a total
row, then totals per model and per tool.

- **`markdown`** prints that document as Markdown headings and tables.
- **`tree`** lists the agents as `tree` would, each line carrying that agent's figures, followed
  by a one-line total.
- **`json`** writes one JSON document with every figure: per agent, its outcome, start and
  final finish times (a resumed child counts only its last completion),
  usage, request counts (requested, completed, failed, interrupted, attempts), compactions, jobs
  created by role, and calls, errors, and unanswered calls per tool; plus per-model and
  per-tool totals and the session totals.

`--list` prints one row per session: id, start time, the initial prompt's first 60 characters,
and the same totals as a report's total row. Sessions written by an earlier session format
are left out.

Agent paths are built from names like directories: the root agent is `/`, and an agent named
`bar` spawned by `/foo` is `/foo/bar`; a later sibling with the same name is `/foo/bar#2`.
Names come from the job that spawned the agent. Model call counts
cover agent turns only, while token figures include compaction requests. Sessions written by an
earlier session format are refused with a nonzero exit.

## Authentication commands

```sh
skyhook auth login
skyhook auth login --headless
skyhook auth status
skyhook auth logout
```

The optional provider argument currently accepts only `codex`, which is the default.
`auth login --headless` selects device authorization rather than a browser; it is separate
from `skyhook batch`. Login needs no configuration file; when one exists it must load, and login
signs in to the `auth_url` its `codex` providers share (entries naming different ones are refused),
or OpenAI's issuer when none names one. `auth status` checks the stored credentials against that
issuer; `auth logout` reads no configuration.
See [authentication](../configuration/authentication.md) for credential ownership and storage.

## Related settings

The [configuration overview](../configuration/overview.md) describes configuration paths
and the full example. [Authentication](../configuration/authentication.md#api-keys-and-environment-files)
documents invocation-directory `.env` loading. [Terminal interface](../guide/terminal-interface.md)
documents slash commands, keybindings, model selection, and saved UI state.

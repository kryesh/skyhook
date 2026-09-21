# Permissions and capabilities

## Approval policy

`--approve-all` or
`approve_all: true` bypasses approval prompts. Subject to the granted capabilities, the default
CLI auto-allows reads and agent operations, as well as writes inside the root workspace.
Execution, HTTP requests, SSH access, target changes, and writes outside the root workspace
require approval. When the `interactive` capability is granted, questions and SSH authentication
appear in the interface. Otherwise operations needing human input fail
immediately; `approve_all` bypasses tool approvals but never restores questions or authentication.

Filesystem tools and command `cwd` accept absolute paths or relative paths including `..`.
The workspace is a base directory, not a security boundary. Filesystem path authorization and
human approval are distinct: the default CLI read policy auto-allows reads even when their
canonical paths are outside the workspace. Do not rely on a confirmation prompt to protect
outside-workspace reads. Outside-workspace writes require confirmation, and remote access has
its own approval requirements.

Choose **Allow once** to approve only the current operation. When offered, **Allow proposed
scope** saves the displayed grant for later operations in the same session, including after
resume; inspect **Details** before granting it. Path grants apply only to the approved target
and access mode. Directory grants cover descendants; read and write grants remain separate.
Process execution remains subject to confirmation on each invocation. These defaults apply to
the CLI; [library hosts](../development/embedding.md) can supply a different approval policy.

Approval controls and pending/granted approval details remain user-facing. Agents see an ordinary
queued job while authorization is pending. A denial preserves its reason and marks the rejected
operation with `code: "permission_denied", executed: false`; it does not undo work that a
containing script already performed.
Agents must not circumvent a denial through another tool or route. For programmatic failure
handling, see the [JavaScript response contract](../reference/javascript.md#responseunwrap-and-native-results).

## Capabilities

Capabilities are granted by the active [mode](#modes), or by a batch job's
`--capabilities` list.

| Capability | Controls | `general` mode |
| --- | --- | --- |
| `read` | File reads, searches, and skill reads | Enabled |
| `write` | File creation, modification, removal, and related write access | Enabled |
| `exec` | `exec` and `shell` | Enabled |
| `network` | HTTP(S) `fetch` | Enabled |
| `targets` | Target-management tools and target-selection inputs | Disabled |
| `ssh_agent` | Targets using `ssh.external_agent`, which forward an SSH agent Skyhook does not own; without it they are hidden | Disabled |
| `agents` | Child-agent creation, also limited by available delegation depth | Enabled |
| `interactive` (runtime only) | Human-facing root questions, approval prompts, and sensitive authentication | Enabled by the terminal host; never in a batch job |
| `mcp` | MCP server startup and MCP tool availability | Enabled |

A capability list is exact; `[]` grants no policy capabilities. Unknown names and `interactive`
are errors in a mode and in `--capabilities`. The host supplies `interactive` independently of the
list: the terminal always has it and `skyhook batch` never does, so an empty list does not disable
the terminal interface or root questions. Per-agent depth restrictions narrow the result. Enable
target support by including `"targets"` in a mode.

## Modes

A mode is a named permission preset. The root agent runs in one mode at a time:

```yaml
# The mode a new session starts in; "general" when omitted.
default_mode: "readonly"

modes:
  readonly:
    capabilities: ["read", "network"]
    instructions: "Investigate and report. Do not change anything."
    hint: "Look things up without changing anything"

  # Built in, and listed first. Declare it only to change what it grants.
  general:
    capabilities: ["read", "write", "exec", "network", "agents", "mcp"]
```

`capabilities` is required, and `default_mode` must name `general` or a declared mode. Optional `instructions` are added to the system prompt of an agent
running in the mode; they guide the model, whereas capabilities are enforced. An optional `hint`
offers the mode to agents starting a child: the `agent` tool lists each hinted mode whose
capabilities the caller holds itself, with those capabilities and the hint. A mode without a hint
cannot be given to a child. A workspace
config that declares a mode replaces the whole definition of a user-config mode with that name;
modes it does not name are kept, in declaration order.

`--mode NAME` selects the starting mode; without it the terminal starts in the mode last used in
that workspace, and a batch job in `default_mode`. In the terminal, `Tab`/`Shift+Tab` in the composer (or
`/mode`) choose the mode the next message is sent in; the footer shows it. A change takes effect
with that message and forfeits the provider's prompt cache for the next request. Child agents keep what they start with: the capabilities and instructions of the mode their
parent chose for them, or otherwise the capabilities their parent holds and no mode instructions.
Background jobs and scripts already running keep their capabilities too. A session can only hold capabilities
that some configured mode grants, and MCP servers start under that union, so switching modes
never starts or stops a server. Approvals granted earlier stay recorded but cannot be used while
the mode lacks their capability. A resumed session continues in the mode it was last in. A
session records each mode's definition the first time it is used and keeps it: editing or
removing that mode in the configuration does not change the session, while modes added later
can still be selected. A resumed session is also never granted more than it started with, so a
batch session reopened in the terminal stays without `interactive`. Because `general` is always
present, redeclare it with a narrower list if the terminal should never be able to switch to its
default capabilities.
Continuing a failed or interrupted root turn also applies a newly chosen mode.

Capabilities gate both tool discovery and invocation, including JavaScript. Approval policies and
cached grants cannot restore a missing capability. Ungated orchestration tools remain available even
with an empty set. These gates are **not an OS sandbox**: allowing `exec` lets a command access files
or the network independently of the corresponding built-in tool gates. Disabling `network` removes
`fetch`, not model-provider traffic or trusted MCP transport setup; disable `mcp` to prevent MCP
connections. Per-server MCP requirements are additional gates, not grants.

See [headless execution](headless.md) for noninteractive commands and authentication,
[execution targets](execution-targets.md) for route approvals, and
[MCP](../configuration/mcp.md) for trusted server startup and per-server gates.

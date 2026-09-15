# Permissions and capabilities

## Approval policy

`--approve-all` or
`approve_all = true` bypasses approval prompts. Subject to the granted capabilities, the default
CLI auto-allows reads and agent operations, as well as writes inside the root workspace.
Execution, remote access, target changes, and writes outside the root workspace require confirmation. When the `interactive` capability is granted, questions
and SSH authentication appear in the interface. Otherwise operations needing human input fail
immediately; `approve_all` bypasses tool approvals but never restores questions or authentication.

Filesystem tools and command `cwd` accept absolute paths or relative paths including `..`.
The workspace is a base directory, not a security boundary. Filesystem path authorization and
human approval are distinct: the default CLI read policy auto-allows reads even when their
canonical paths are outside the workspace. Do not rely on a confirmation prompt to protect
outside-workspace reads. Outside-workspace writes require confirmation, and remote access has
its own approval requirements.

Path grants are cached in memory by session, target, access mode, and path; directory grants
cover descendants, and read and write grants remain separate. Process execution remains subject
to confirmation on each invocation. Applications embedding the core can supply a different
approval policy, including one that prompts for or denies reads.

Approval controls and pending/granted approval details remain user-facing. Agents see an ordinary
queued job while authorization is pending. Denials preserve the reason and add
`code: "permission_denied", executed: false` to the rejected operation's error or job envelope;
script exceptions carry the same fields. A containing script may already have executed other work,
so uncaught script failures retain the rejected operation as nested failure details. Agents must not
circumvent a denial through another tool or route.

## Capabilities

The top-level configuration has one exact policy-capability allowlist:

```toml
# These are the defaults when capabilities is omitted. Add "targets" to enable targets.
capabilities = ["read", "write", "exec", "network", "agents", "mcp"]
```

| Capability | Controls | Default |
| --- | --- | --- |
| `read` | File reads, searches, and skill reads | Enabled |
| `write` | File creation, modification, removal, and related write access | Enabled |
| `exec` | `exec` and `shell` | Enabled |
| `network` | HTTP(S) `fetch` | Enabled |
| `targets` | Target-management tools and target-selection inputs | Disabled |
| `ssh_agent` | Targets using `ssh.external_agent`, which forward an SSH agent Skyhook does not own; without it they are hidden | Disabled |
| `agents` | Child-agent creation, also limited by available delegation depth | Enabled |
| `interactive` (runtime only) | Human-facing root questions, approval prompts, and sensitive authentication | Enabled by the terminal host; disabled in headless mode |
| `mcp` | MCP server startup and MCP tool availability | Enabled |

An explicit array replaces the defaults; `capabilities = []` grants no policy capabilities.
Unknown names and `interactive` are errors in this list and in `--capabilities`.
`--capabilities read,write` replaces the config allowlist for that invocation; `--capabilities=`
selects an empty policy set. Resolution is **CLI allowlist > config allowlist > defaults**.
The host then supplies `interactive` according to the runtime mode, independently of the allowlist,
and per-agent depth restrictions narrow the result. Use `--non-interactive` to disable human
interaction; an empty allowlist does not disable the terminal interface or root questions.
Overrides also apply when resuming or creating another session in the interface. Enable target
support by including `"targets"` in the allowlist.

Capabilities gate both tool discovery and invocation, including JavaScript. Approval policies and
cached grants cannot restore a missing capability. Ungated orchestration tools remain available even
with an empty set. These gates are **not an OS sandbox**: allowing `exec` lets a command access files
or the network independently of the corresponding built-in tool gates. Disabling `network` removes
`fetch`, not model-provider traffic or trusted MCP transport setup; disable `mcp` to prevent MCP
connections. Per-server MCP requirements are additional gates, not grants.

See [headless execution](headless.md) for noninteractive commands and authentication,
[execution targets](execution-targets.md) for route approvals, and
[MCP](../configuration/mcp.md) for trusted server startup and per-server gates.

# MCP servers

Configure Model Context Protocol servers in the top-level **`mcp`** map, with one named
`[mcp.<name>]` table per server. The map defaults to empty: no MCP connections, tools, or MCP prompt
guidance are added unless servers are configured. Skyhook supports **stdio** and **Streamable HTTP**;
legacy HTTP+SSE transport is not supported.

```toml
[mcp.files]
transport = "stdio"
start_command = ["my-mcp-server", "--root", "/srv/project"]
capabilities = ["read"]
cwd = "mcp-work"                    # Relative to the source config file's directory.
env = { LOG_LEVEL = "warn" }       # Environment overrides for the launched process.
startup_timeout_secs = 30           # Default; must be a positive integer.
call_timeout_secs = 120             # Default; must be a positive integer.

[mcp.service]
transport = "streamable_http"
url = "http://127.0.0.1:8080/mcp"    # Full MCP endpoint, not an API root.
capabilities = ["read", "write"]
headers_env = { Authorization = "MY_MCP_AUTHORIZATION" }
# Optional local startup fallback, only when the endpoint is unreachable:
# start_command = ["my-http-mcp-server", "--port", "8080"]
```

## Connections and credentials

`start_command` is an argument vector, not shell text; its first element must name a nonempty
executable. Stdio requires it and rejects `url` and `headers_env`. Streamable HTTP requires an
absolute HTTP(S) `url`. It connects first, and only starts the optional command if the endpoint is
unreachable—not on authentication, protocol, or other server errors. `cwd` and `env` apply only to
`start_command`; HTTP connections without a startup command must omit them. Relative `cwd` is
resolved against the directory of the source configuration file that defines the value, not the
agent workspace. When user and workspace config are layered, an inherited user `cwd` retains its
user-file base; an overlaid workspace `cwd` uses `<workspace>/.skyhook`. Timeouts must be positive
whole seconds. Unknown settings and invalid transport combinations are rejected.

HTTP `headers_env` maps header names to environment-variable names. The variable's complete value
is sent as the header (for example, `MY_MCP_AUTHORIZATION` can contain `Bearer ...`); secrets do not
belong in TOML. Missing or non-Unicode variables, invalid header names, and values that cannot be
encoded as HTTP headers prevent that server's connection from starting. Empty header values are
passed through to the server. Do not put secrets in `start_command` arguments or literal `env` values.

## Trust and permissions

**Trust boundary:** configured startup commands are trusted configuration, including commands
from a workspace `.skyhook/config.toml` overlay. Review workspace config before use. They run during
session startup without a model tool-approval prompt, only on the local root/session host—not on
selected SSH targets. Only configure commands and endpoints you trust. Child agents share the
session's MCP connections rather than starting their own servers.

The session must hold the global **`mcp` capability** before any MCP server is launched or contacted,
and before any MCP tool can be exposed or invoked. Each server's `capabilities` array defaults to
`[]` and adds requirements to that global gate. Valid names are `read`, `write`, `exec`, `network`,
`targets`, `ssh_agent`, `agents`, `interactive`, and `mcp`. An agent must hold **all** listed capabilities to see
or call a server's tools; otherwise those tools are hidden from both the model catalog and
JavaScript access. Servers whose requirements exceed the session's capabilities are not contacted
or launched at all. This is a host-configured gate, not an inference from server annotations.
Nonempty lists also request those permissions through the normal approval policy, scoped to the
MCP server/tool rather than a filesystem workspace. The implicit global `mcp` requirement is
availability-only and introduces no approval prompt. `approve_all` never bypasses missing
capabilities. **An omitted or empty per-server list requires only global `mcp` and produces no
permission prompt.** MCP transports do not implicitly require `exec` or `network`; startup
commands and endpoints remain trusted host configuration.

## Using imported tools

Skyhook imports **tools only**, not MCP prompts or resources. At startup it initializes each server
and discovers its tools, then freezes that catalog for the session lifetime; later catalog-change
notifications do not add or replace tools. A server that fails startup/discovery is warned about and
skipped rather than preventing the session from starting. The terminal
interface's sidebar shows each server's startup status.

Imported tools are available to the model and through `tool` builders inside `script`. Use the
exact advertised name and schema rather than deriving them from the server's tool name. Names
usually look like `mcp_filesystem_write_file`, but may have a suffix to avoid conflicts. Arguments
may appear at the top level or under an `arguments` object so they do not conflict with common
Skyhook arguments such as `bg`. Tools with invalid or unsupported input schemas are warned about
and skipped individually.

Call a tool with `tool[name](...)` or its fluent builder. Calls return the
[common JobView envelope](../reference/javascript.md#jobview-response-contract); the MCP payload
(`content`, plus optional `structuredContent` and `isError`) is in `.result`. Images use the usual
[saved-output image handling](../reference/javascript.md#saved-output-and-serialization), and server-reported errors retain their
output in failed jobs.

Calls pass through the same cancellation and background-job machinery as builtins. Skyhook
bounds discovery and response sizes, and does not replay a call with an uncertain outcome after
a connection failure. Cancellation cannot undo effects that already occurred on the server.

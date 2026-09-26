# Authentication

## API keys and environment files

Any configured value that may hold a secret—`api_key` and an entry's fixed `headers`—takes one
of three forms: a literal, `env: NAME` to read an environment variable, or `command: "..."` to
run a command:

```yaml
providers:
  anthropic:
    codec: "messages"
    dialect: "anthropic"
    base_url: "https://api.anthropic.com/v1"
    api_key:
      env: "ANTHROPIC_API_KEY"
    # Alternatively:
    # api_key: "sk-ant-..."
    # api_key:
    #   command: "op read 'op://Private/Anthropic/api-key'"
    headers:
      x-title: "skyhook"
      x-proxy-token:
        env: "PROXY_TOKEN"
```

A literal secret is stored in plain text in the configuration file, and `skyhook dump config`
prints it. Prefer `env` or `command` for secrets.

Environment-variable values the provider sends are resolved when it is built and must be present
and nonblank. They are used as they are, not trimmed: a value with a trailing newline fails,
naming the provider and field. A command runs only on the first model request that sends its value, not when
configuration is loaded or a conversation is opened. A successful command's stdout is decoded as
UTF-8 and trimmed of leading/trailing whitespace and newlines, then cached in memory for that
provider instance. Concurrent requests and child conversations share the cache; a new process or
provider instance resolves the value again. Failed commands are not cached and may be retried on a
subsequent request. When the server refuses a cached value with HTTP 401, the value is discarded
and the next request runs the command again. If the refused value had been accepted before, as
when a short-lived token expires, the model request is retried automatically; a value refused on
its first use fails the request. Empty output, invalid UTF-8, output that is not a valid header
value, and nonzero exits fail the request without including command output in the error. Stdout
is limited to 64 KiB. Commands run before the HTTP startup timeout; there is no separate command
timeout.
Cancelling the invocation terminates the command's immediate child process, but does not guarantee
termination of any descendants it launched.

Commands are trusted host configuration, executed with `/bin/sh -c`, inheriting Skyhook's process
working directory and environment, not an agent's workspace or remote target. They do not run
through tool approval. Standard input is closed and standard error is discarded; use a noninteractive
secret-manager command that writes only the key to stdout. Do not put literal secrets in command
strings or commit them to configuration files.

At CLI startup, Skyhook loads **`.env` in the invocation directory** before constructing providers.
Existing process environment variables take precedence. A missing file is ignored; Skyhook does
not search parent directories, the `--workspace` directory, or the configuration file's directory.
Loaded variables are also inherited by credential commands and other **local** child processes.
Neither inherited host environment variables nor `.env` values are automatically forwarded into
remote target processes; remote commands use the remote machine's environment. Managed SSH routes
disable `SendEnv`, `SetEnv`, and X11 forwarding. Local SSH authentication/proxy helpers still use
the host environment; Skyhook's [SSH-agent forwarding](../guide/execution-targets.md#agents) is
configured separately.

For example, in the directory from which you run `skyhook`:

```dotenv
ANTHROPIC_API_KEY="your-key"
```

Keep `.env` out of version control and restrict its file permissions. The CLI supports normal dotenv
quoting, comments, `export` declarations, and variable interpolation. A malformed or unreadable file
fails startup without printing its contents.

## Codex subscription login

The `codex` provider uses **Skyhook-owned** OAuth credentials. Run `skyhook auth login` for browser
authorization or `skyhook auth login --headless` for device authorization. `skyhook auth status`
reports local login status for the configured issuer and `skyhook auth logout` removes Skyhook's
credentials. Login does not require a model configuration; when the discovered config has a
`codex` provider with `auth_url`, login and token refresh use that issuer instead of
`https://auth.openai.com`, and `base_url` likewise replaces `https://chatgpt.com/backend-api/codex`
for requests. `auth_url` must use HTTPS (HTTP only on a loopback host) and carry no credentials,
query, or fragment; a path prefix is kept. A configuration whose `codex` providers name different
issuers is refused: one login serves one issuer. Skyhook never imports, reads, or modifies the
official Codex client's credential files. Secure Codex credential storage currently requires Unix;
other platforms fail explicitly rather than writing tokens without private file permissions.

Stored credentials are bound to the issuer that granted them. When a provider's issuer differs, or
the credentials were saved by an older Skyhook, requests fail without sending them and ask you to
run `skyhook auth login codex` again.

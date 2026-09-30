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
prints it. Prefer `env` or `command` for secrets. `api_key` and `headers` are provider-only:
all models in that provider instance share their credential resources, even when their codecs or
request settings differ. A LiteLLM virtual key is sent as a bearer token for every codec,
including `messages`.

Environment-variable values the provider sends are resolved when it is built and must be present
and nonblank. They are used as they are, not trimmed: a value with a trailing newline fails,
naming the provider and field. A command runs only on the first model request that sends its value, not when
configuration is loaded or a conversation is opened. A successful command's stdout is decoded as
UTF-8 and trimmed of leading/trailing whitespace and newlines, then cached in memory for that
provider instance. All its models, concurrent requests, and child conversations share the cache;
a new process or provider instance resolves the value again. Failed commands are not cached and
may be retried on a subsequent request. When the server refuses a cached value with HTTP 401, the value is discarded
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

A `codex` provider uses **Skyhook-owned** OAuth credentials, one login per provider. Name the
provider as it is configured: for a provider named `codex`, run `skyhook auth login codex` for browser
authorization or `skyhook auth login codex --headless` for device authorization.
`skyhook auth status codex` reports that provider's local login status without contacting the
service, so it cannot tell whether the service still accepts the login. `skyhook auth status codex
--check` also presents the credentials to the service with the provider's `headers`, refreshing
them first if a request would, and reports the plan and rate-limit use it returns. It fails when
the provider is not logged in or the service rejects the credentials. The check reads the account's
usage, which runs no model and spends no quota; that endpoint is not publicly documented by OpenAI, so what it
reports may change. Several `codex` providers can sign in to different accounts or issuers
independently. Login does not require the provider to have models.

When the provider sets `auth_url`, login and token refresh use that issuer instead of
`https://auth.openai.com`. `base_url` likewise replaces the ChatGPT backend root,
`https://chatgpt.com/backend-api`. Requests go to `codex/responses` beneath it, and `--check` to
`wham/usage`, so a mirror serves both beneath one root. `auth_url` must
use HTTPS (HTTP only on a loopback host) and carry no credentials, query, or fragment; a path prefix
is kept. Skyhook never imports, reads, or modifies the official Codex client's credential files.
Secure Codex credential storage currently requires Unix; other platforms fail explicitly rather than
writing tokens without private file permissions.

Credentials are kept in a `credentials/` directory next to the configuration file that defines the
provider, as `<provider>.json`. For a provider in the user configuration that is
`~/.config/skyhook/credentials/` (or under `$XDG_CONFIG_HOME/skyhook`). For a provider the workspace
configuration defines, it is `<workspace>/.skyhook/credentials/`; keep that directory out of version
control. For `--config path.yaml`, it is beside that file. Skyhook creates the directory when you
first log in, readable only by you, and refuses to use one that grants any access to other users.
Checking status or starting a session creates nothing there, except a missing lock file beside saved
credentials.

To sign a provider out, delete its `credentials/<provider>.json` while no session is using it.
Nothing is revoked on the server, so this is also how to clear credentials for a provider you have
removed from your configuration.

A provider name becomes a file name, so it must contain no NUL character and be at most 250 bytes.
On a case-insensitive file system, such as macOS by default, names that differ only in case (`Work`
and `work`) share one credentials file; give such providers distinct names.

Stored credentials are bound to the issuer that granted them. When a provider's issuer differs,
requests fail without sending them and name the command that signs it in again. That command
selects the provider's configuration file with `-c`, so it signs in the same entry the session used
wherever you run it.

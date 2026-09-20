# Authentication

## API keys and environment files

Reference an environment variable with `api_key_env`, or use a command to retrieve the key:

```toml
[providers.anthropic]
kind = "anthropic"
base_url = "https://api.anthropic.com/v1"
api_key_env = "ANTHROPIC_API_KEY"
# Alternatively, remove api_key_env and use:
# api_key_command = "op read 'op://Private/Anthropic/api-key'"
```

`api_key_env` and `api_key_command` are mutually exclusive. Environment-variable keys are resolved
when the provider is built and must be present and nonblank. Commands are run only on the provider's
first model request, not when configuration is loaded or a conversation is opened. A successful
command's stdout is decoded as UTF-8 and trimmed of leading/trailing whitespace and newlines, then
cached in memory for that provider instance. Concurrent requests and child conversations share the
cache; a new process or provider instance resolves the key again. Failed commands are not cached
and may be retried on a subsequent request. Empty output, invalid UTF-8, invalid credential headers,
and nonzero exits fail the request without including command output in the error. Stdout is limited
to 64 KiB. Commands run before the HTTP startup timeout; there is no separate command timeout.
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
Loaded variables are also inherited by API-key commands and other **local** child processes. Neither
inherited host environment variables nor `.env` values are automatically forwarded into remote target
processes; remote commands use the remote machine's environment. Managed SSH routes disable `SendEnv`,
`SetEnv`, and X11 forwarding. Local SSH authentication/proxy helpers still use the host environment;
Skyhook's [SSH-agent forwarding](../guide/execution-targets.md#agents) is
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
reports local login status and `skyhook auth logout` removes Skyhook's credentials. Login does not
require a model configuration. Skyhook never imports, reads, or modifies the official Codex client's
credential files. Secure Codex credential storage currently requires Unix; other platforms fail
explicitly rather than writing tokens without private file permissions.

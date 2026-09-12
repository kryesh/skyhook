# Headless execution

Use `--non-interactive` with exactly one initial prompt or script:

```sh
session_id=$(skyhook --non-interactive -p "Review this repository" --capabilities read,agents)
skyhook --non-interactive -s workflow.js --approve-all
skyhook --non-interactive --resume "$session_id" -p "Summarize the findings"
```

Headless mode does not require a terminal, read answers from stdin, or load TUI themes/keybindings.
It creates or opens the session, prints **only its session ID followed by a newline to stdout**, and
flushes that line before executing the prompt or workflow. It then exits automatically. The ID lets
external programs locate and follow the normal session logs under the configured `session_root`
(default: `<workspace>/.skyhook/sessions`). Resumed runs print the existing session ID.

Assistant output, script console output/results, startup warnings, and execution diagnostics remain
in the session logs; they are not printed to stdout or stderr. The process exits successfully when
the submitted operation and shutdown succeed, and nonzero on failure or interruption. A failure
before a session can be opened produces no session-ID line. Explicit `--help` and `--version`
retain their normal output. Headless mode is not a line-oriented conversation over stdin.

The submitted root turn or workflow defines completion. A workflow must explicitly await background
work it needs completed; outstanding jobs are cancelled and drained during shutdown. Interrupt and
termination signals also trigger cleanup and journaled status rather than a terminal prompt.

`--non-interactive` always disables human interaction. Root `ask` is unavailable (also inside scripts and with `bg:true`). Child agents
can still ask their owning parent agent. Operations requiring human approval fail immediately;
ordinary automatically allowed operations still work. `--approve-all` (or config `approve_all = true`)
bypasses tool approvals but does not enable questions, SSH passwords/passphrases, or host/agent
confirmation prompts. SSH credentials that work without a prompt can still authenticate.

Without `interactive`, `exec` and `shell` run in a new process session with no controlling terminal,
so ordinary `/dev/tty` prompts cannot stop the job waiting for terminal input. Each command forces a
Skyhook-owned rejecting askpass helper over inherited `SSH_ASKPASS` settings, even when `targets` is
disabled. Existing `SSH_AUTH_SOCK` credentials are preserved unless the configured target-authentication
setup replaces them. Remote tool requests carry the caller's exact capabilities, so remote commands
apply the same restrictions; incompatible shim protocol versions are rejected rather than falling
back to default capabilities. This is still not an OS sandbox against deliberately programmed
subprocesses that establish their own external interaction mechanisms.

Askpass sockets and helpers live in uniquely created `skyhook-askpass-<pid>-<random>` temporary
directories. No fixed socket pathname is shared across Skyhook instances, or even across concurrent
askpass servers in one instance. Directories and helpers have explicit `0700` permissions and sockets
have `0600` permissions independent of umask; the listener accepts only peers with the same effective
UID. Each owner removes only its own socket/helper/directory during cleanup.

See the [CLI reference](../reference/cli.md) for argument combinations and
[permissions](permissions.md) for the capability/approval distinction.

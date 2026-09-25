# Headless execution

Use `skyhook batch` with exactly one prompt or script:

```sh
session_id=$(skyhook batch -p "Review this repository" --capabilities read,agents)
skyhook batch -s workflow.js --approve-all
skyhook batch -p "What does this service do?" --mode readonly
skyhook batch --resume "$session_id" -p "Summarize the findings"
```

A batch job runs in the default [mode](permissions.md#modes), in the one `--mode` names, or
with the exact `--capabilities` list. Headless mode does not require a terminal or read answers from stdin.
It creates or opens the session, prints **only its session ID followed by a newline to stdout**, and
flushes that line before executing the prompt or workflow. It then exits automatically. The ID lets
external programs locate and follow the normal session logs under `<workspace>/.skyhook/sessions`.
Headless mode uses only the selected workspace's history, just like the terminal UI.
Resumed runs print the existing session ID.

Assistant output, script console output/results, startup warnings, and execution diagnostics remain
in the session logs; they are not printed to stdout or stderr. The process exits successfully when
the submitted operation and shutdown succeed. On failure or interruption it exits nonzero and
prints the error to stderr; stdout still carries only the session ID. A failure before a session
can be opened produces no session-ID line. Explicit `--help` and `--version`
retain their normal output. Headless mode is not a line-oriented conversation over stdin.

The submitted root turn or workflow defines completion. A workflow must explicitly await background
work it needs completed; outstanding jobs are cancelled and drained during shutdown. Interrupt and
termination signals also trigger cleanup and journaled status rather than a terminal prompt.

A batch job never has human interaction. Root `ask` is unavailable (also inside scripts and with `bg:true`). Child agents
can still ask their owning parent agent. Operations requiring human approval fail immediately;
ordinary automatically allowed operations still work. `--approve-all` (or config `approve_all: true`)
bypasses tool approvals but does not enable questions, SSH passwords/passphrases, or host/agent
confirmation prompts. SSH credentials that work without a prompt can still authenticate.

`exec` never has stdin connected; without `interactive` it also has no
controlling terminal, so
ordinary `/dev/tty` prompts cannot stop the job waiting for terminal input. SSH authentication
prompts are rejected even when inherited `SSH_ASKPASS` settings request another prompt helper
or the `targets` capability is disabled. Existing `SSH_AUTH_SOCK` credentials are preserved unless
the configured target-authentication setup replaces them. Remote commands apply the same
capability restrictions. This is still not an OS sandbox against deliberately programmed
subprocesses that establish their own external interaction mechanisms.

See the [CLI reference](../reference/cli.md) for argument combinations and
[permissions](permissions.md) for the capability/approval distinction.

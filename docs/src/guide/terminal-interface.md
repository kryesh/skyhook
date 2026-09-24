# Terminal interface

## Conversation and inspector

User messages appear on the right and assistant messages on the left. Tool previews show their
remote execution target after the tool name, such as `exec @lab-monitoring`; local calls omit
`@root`. Expand a tool call to inspect its arguments, output, or failure details. Scripts,
commands, file content, and diffs have syntax highlighting. Click an expanded body to collapse
it, or drag to select text. Copying code preserves its contents without visual padding or
newlines added by wrapping. Language-labelled code blocks in messages and reasoning receive
syntax highlighting once their closing fence arrives; open blocks keep a consistent plain
code style while streaming.

The agent tree appears above the composer while children are active or a child agent is being
viewed. Click an agent to inspect its conversation without mixing its output with other agents.
Each agent retains its reading position and expanded rows. Agents using the `wait` tool show
**Waiting** rather than **Running tools**. Tree rows show `@target` for remote agents and their
own token totals and context usage: output · input (uncached) · context.
Agent call previews show the child's target even while queued or running; an omitted target
inherits the caller's target. Completed final replies identify the model that answered in a
footer below the reply.

The composer always sends to the root agent. While root is busy, Enter queues a follow-up
for its next model request, without interrupting the current request or tools. It does not wait
for the entire turn to finish. `/queue` edits/removes input that has not yet been consumed.
Selecting a queued message to edit pauses automatic queue delivery and moves it into the
composer; merely opening the queue or deleting an item does not pause it. Interruption,
submission or delivery errors, and session-start failures also pause queue delivery. `/resume`
clears that pause, as does normal composer Enter (even with an empty draft); slash commands
do not automatically resume the queue.
Queued input lives only in the running interface: it is not saved with the session. Each open
session keeps its own queue, which keeps delivering while you look at another session and is
discarded when its session is closed.

`/retry` continues failed or interrupted turns without duplicating their original prompts.
After a session interruption it resumes all interrupted children automatically, regardless of
which agent is selected. Parents with pending waits stay in those waits.
Automatic model retries display the attempt number, next delay, and latest diagnostic in one
error block. Partial output from the failed attempt clears when the next attempt starts;
successful replies have no retry labels. Retries continue until success or cancellation for
[transient model failures](../configuration/providers-and-models.md#model-failure-recovery).

The inspector provides Conversation, Requests, and Jobs tabs. Switching agents keeps the selected
tab, while reading positions and expanded rows remain separate for each agent. Requests include
compaction requests and token summaries; transient retries appear under the same request, which
reads **Retrying** while the next attempt is pending. Full request bodies are available in the
session log, not rendered in the UI. Status messages, including
skill warnings, interruptions, and errors, appear in the conversation and are saved with the
session, but are not sent to the model. Connection and MCP startup warnings describe this
installation rather than the session, so they are shown but not saved.

Job output is paged and searchable without acknowledging the agent's pending notifications.
Select a job and press `o` for output fields, regex search, and the next page; `c` requests
cancellation (useful for background jobs, since interrupting already cancels the foreground tool).
Intermediate child-agent messages and terminal job notifications appear as expandable **Job event**
cards. Earlier progress messages remain readable after the job completes or resumes. Remote
output becomes available as it is transferred to the host.

Provider-supplied reasoning streams in a separate expanded block and collapses when answer text
starts or the response finishes. Click a multi-line block or press Enter when selected to reopen
it, including after resuming a session. Single-line reasoning stays inline without an
expand/collapse control and is not selectable, even when it wraps in a narrow terminal.
Spinners indicate that a request is still active.

## Model selection and UI state

Models are listed in configuration declaration order. For a new session, selection uses
`--model`, then the most recently submitted configured model, then the first model in the list.
The starting [mode](permissions.md#modes) is chosen the same way: `--mode`, then the mode of the
most recently submitted message if it is still configured, then `default_mode`. Batch jobs
ignore the remembered mode.
`/model` (or `Ctrl+X M`)
selects the model for subsequent user messages in the current session. Selection stays in the UI
until a message is sent; cancelling the picker or leaving without sending does not change the
session's recorded model. Each submitted message captures its model, including queued messages.
Queued messages are submitted together as one batch, in order, including their attachments.
The batch cannot be split across requests; the last message's captured model is used for that request.
Tool follow-ups, retries, compaction, and `/retry` retain the active turn's model. The bottom bar
shows the choice for the next message; reply footers identify the model that actually answered.
Resumed sessions retain their last applied model. `/models` remains an alias for `/model`.
Restore a missing recorded model profile before resuming rather than substituting another model.

The interface stores the last submitted model and mode and the sidebar setting in the workspace's
`.skyhook/state.json`, beside its sessions, without changing the model configuration.
Session titles are saved with their history.

A sidebar beside the conversation lists each configured MCP server with its startup status and the
viewed agent's todos, with the viewed agent's capabilities pinned to its bottom. For the root agent
these are what the mode of the next message grants. A section with nothing to list
is left out, title included. The sidebar is shown by default; `Ctrl+X B` or `/sidebar` toggles it.
Terminals narrower than 100 columns hide it without changing the saved setting.

The bottom bar uses this format:

```text
default · openai       8.4k · 200k(31.2k) · 42% (54k/128k)
```

Values are session output tokens, session total input tokens (uncached input), and the selected
agent's estimated current context occupancy (current tokens/model capacity). Session totals
include children and compaction. Context includes instructions, tools, history, and runtime
state; it is not cumulative usage. Missing context data appears as `—`.

## Keyboard and mouse

The command palette and `/help` also list these shortcuts. `Ctrl+X` is a leader: release it, then press the next key when ready, or `Esc` to cancel.

| Shortcut | Action |
| --- | --- |
| `Ctrl+P`, `/` | Commands |
| `Ctrl+X N`, `Ctrl+X S`, `Ctrl+X W` | New session, switch session, close session |
| `Ctrl+X M`, `/model` | Model for subsequent user messages |
| `Ctrl+X A` | Agent picker |
| `Ctrl+X I`, `/queue` | Edit queued follow-ups |
| `Ctrl+X C`, `/retry` | Continue failed or interrupted turns |
| `Ctrl+X T`, `/details` | Toggle tool details |
| `Ctrl+X F`, `@` | Attach a workspace file |
| `Ctrl+X B`, `/sidebar` | Toggle the sidebar |
| `Ctrl+X R`, `/attention` | Reopen pending questions and permissions |
| `Ctrl+X ←`, `Ctrl+X →` | Previous/next of Conversation, Requests, Jobs (wrapping) |
| `Ctrl+X ↑`, `Ctrl+X ↓` | View the agent above/below in the tree (wrapping) and focus the tree |
| `↑`, `↓` in the tree | View the agent above/below at its latest activity |
| `Ctrl+X Y`, `Ctrl+X X` | Copy message/selection, export conversation |
| `Tab`, `Shift+Tab` | Next/previous [mode](permissions.md#modes) in the composer; next/previous row in the tree or conversation |
| `Ctrl+X P`, `/mode` | Choose the mode from a list |
| Click a pane | Focus composer, tree, or conversation |
| `Enter` | Send/queue, select, expand |
| `Alt+Enter`, `Ctrl+J`, supported `Shift+Enter` | Newline |
| `PageUp`, `PageDown` | Scroll history |
| `Ctrl+Alt+U`, `Ctrl+Alt+D` | Half-page scrolling |
| `Home`, `End` in content | Beginning, latest |
| `/`, `n`, `N` in content | Search, next/previous match |
| `[`, `]` in content | Previous/next inspector tab |
| `Esc` | Close local UI, dismiss/cancel prompts, otherwise interrupt work, cancelling the running foreground tool |
| `Ctrl+C` | Clear draft, otherwise interrupt (as `Esc`)/quit |
| `Ctrl+X H`, `/help` | Help and shortcuts |
| `Ctrl+X Q` | Quit |

The `Ctrl+X` preview stays open until the next key; `Esc` cancels it. It shows Model
and Inspect agent, plus Questions when requests are pending, Sessions when another open
session needs attention, and Edit queue when queued messages are available. Other bound shortcuts still work even when hidden
from the preview.

Type `/` in an empty composer to open the command palette. Search by command name
(with or without `/`), label, or configured shortcut; exact command names take priority over label matches.
The command palette omits navigation-only actions; the `Ctrl+X` arrows step through
agents and inspector tabs. Click the conversation, or pick an agent with `Ctrl+X A`, to focus it.
Menus use arrows, mouse hover, the mouse wheel, or `Ctrl+P/N`; Enter or Tab activates
the selected item. The Agents
palette labels its Output, Input (uncached), and Context statistics. The composer
wraps at word boundaries and supports word movement, selection, `Ctrl+A/E`, `Ctrl+W`,
`Ctrl+U/K`, and undo/redo with `Ctrl+-` / `Ctrl+.`. Up/Down moves through displayed input
rows, reaching prompt history only from the first/last row.
Click agent and tool rows, scroll the relevant panel, or drag across text in user/agent messages
and tool output, then copy the selected characters with `Ctrl+X Y` (or `y` while content is focused).
Selection supports parts of a line and multiple lines; copying preserves Unicode and code indentation
without adding newlines at visual wraps.
The workspace path is centered in the top bar. The bottom bar shows the model ID, session ID,
and token statistics. These are plain text; use the terminal emulator’s selection gesture
(usually Shift-drag) and copy shortcut. Copy uses the terminal's OSC 52 clipboard support. `@` or `Ctrl+X F` attaches a workspace file
(press `Esc` to keep a typed `@`): image files attach as images and other files as text.
Attachments are listed below the composer, and attached text is sent after the message. Pastes longer than 12 lines appear as inline items at the cursor. Type
before, between, or after multiple paste items; move across, select, delete, and undo them as
single editing units. Sending or copying expands their original contents in place. Shorter pastes
remain ordinary editable text. Use `/attachments` to inspect or remove pasted items and attachments.
Questions and permissions open even while inspecting the agent tree or
conversation; open menus and search keep input focus until closed. `Esc` dismisses foreground
questions and permissions without answering them; reopen pending requests with `Ctrl+X R`
(or `/attention`, labelled **Reopen questions and permissions** in the palette).
Submitting a new prompt rejects any pending question batches; those batches can no longer be reopened.
For background questions and SSH authentication/askpass prompts, `Esc` cancels the request;
cancelled requests cannot be reopened. SSH authentication prompts take priority over questions,
permissions, menus, and search; interrupted question drafts resume afterward.

Within questions and permissions, `↑`/`↓` selects an answer, `PageUp`/`PageDown` scrolls
the prompt text, and `Ctrl+PageUp`/`Ctrl+PageDown` scrolls long answer descriptions.
In multi-question prompts, `←`/`→` switches questions while navigating choices, preserving
each question's selected option and unfinished text. Typing enters text-editing mode, where
`←`/`→` moves the cursor; `Tab` switches between editing and question navigation. `Enter`
confirms an answer and advances to an unanswered question; switching alone never submits.
The mouse wheel scrolls the text or choices beneath the pointer. Drafts remain intact while
answering questions or inspecting details.
Question choices are suggestions: you can select one, optionally add a comment, or provide
a free-form answer. For answering agent questions from a script, see
[jobs and agents](../scripting/jobs-and-agents.md#delegation-and-child-input).

See [sessions and context](sessions-and-context.md) for compaction and persistence,
and the [CLI reference](../reference/cli.md) for startup options.

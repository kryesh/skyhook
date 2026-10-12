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
Each agent and tab retains its reading position and expanded rows. Agents using the `wait` tool
show **Waiting** rather than **Running tools**. Tree rows show `@target` for remote agents and their
own token totals, context usage and time: output · input (uncached) · context · time. The time is
how long a busy agent's current turn has run; once it isn't busy, how long a child ran its job, or
how long the root agent's last turn took.
Agent call previews show the child's target even while queued or running; an omitted target
inherits the caller's target. Completed final replies identify the model that answered in a
footer below the reply, with how long that response took and, when tool rounds came first, how
long the whole turn took.

The composer always sends to the root agent. While root is busy, Enter queues a follow-up
for its next model request, without interrupting the current request or tools. It does not wait
for the entire turn to finish. `/queue` edits/removes input that has not yet been consumed.
Selecting a queued message to edit pauses automatic queue delivery and moves it into the
composer; merely opening the queue or deleting an item does not pause it. Interruption,
submission or delivery errors, and session-start failures also pause queue delivery. `/resume`
clears that pause, as does normal composer Enter (even with an empty draft); slash commands
do not automatically resume the queue.
If Skyhook cannot write a session's journal, that session stops as if the process had crashed:
its queued input is dropped and its conversation shows why. It stays open to read until you close
it, and reopening it continues from what was saved.
Queued input lives only in the running interface: it is not saved with the session. Each open
session keeps its own queue, which keeps delivering while you look at another session and is
discarded when its session is closed.

`/retry` continues failed or interrupted turns without duplicating their original prompts.
After a session interruption it resumes all interrupted children automatically, regardless of
which agent is selected. Parents with pending waits stay in those waits. On a reopened session
that has not run anything yet, `/retry` always starts it and continues whatever it left
unfinished, or reports that there was nothing.
Automatic model retries display the attempt number, a countdown to the next attempt, and the
latest diagnostic in one error block. Partial output from the failed attempt clears when the next
attempt starts; successful replies have no retry labels. Retries continue until success or
cancellation for [transient model failures](../configuration/providers-and-models.md#model-failure-recovery).

The inspector provides Conversation, Requests, and Jobs tabs. Switching agents keeps the selected
tab, while reading positions and expanded rows remain separate for each agent and tab. Requests
include compaction requests, when each was sent, token summaries, and how long each took; transient
retries appear under the same request, which reads **Retrying** while the next attempt is pending.
Full request bodies are available in the session log, not rendered in the UI. Status messages,
such as interruptions and errors, appear in the conversation and are saved with the session, but
are not sent to the model. Startup warnings (skills, configuration, connections and MCP servers)
describe this installation rather than the session, so the terminal shows them but does not save
them. Unsaved notices appear where they arrived in the conversation and scroll away as it grows.
Feedback on an action, such as an attached file, a search without matches, an export, or a
setting that could not be saved, shows briefly on the notice line instead.

Job output is paged and searchable without acknowledging the agent's pending notifications.
Select a job and press `o` for output fields, regex search, and the next page; `c` requests
cancellation (useful for background jobs, since interrupting already cancels the foreground tool).
Intermediate child-agent messages and terminal job notifications appear as expandable **Job event**
cards. Earlier progress messages remain readable after the job completes or resumes. Remote
output becomes available as it is transferred to the host.

Provider-supplied reasoning streams in a separate expanded block and collapses when answer text
starts or the response finishes. Click a multi-line block or press Enter when selected to reopen
it, including after resuming a session. Single-line reasoning stays inline without an
expand/collapse control and is not selectable, even when it wraps in a narrow terminal. A
heading or wholly bold or italic line followed by a blank line starts a new reasoning section,
titled by that line and collapsible like any multi-line block; only the section still streaming
stays expanded. A title with nothing under it stays inline.
Spinners indicate that work is still active, and a counter at the end of the row shows for how
long. The working indicator counts the whole turn, across model requests and tool rounds;
streaming reasoning counts from its request. Running tool calls count up and keep their duration
once finished; expanding one shows when it started. A running tool waiting for input keeps its
status symbol while its counter runs, and a retry counts down to its next attempt. Copied and
exported text includes finished durations but not running counters.

## Model selection and UI state

Models are named `provider/model` and listed in configuration declaration order. For a new
session, selection uses `--model`, then the most recently submitted configured model, then
`default_model`, else the first model of the first provider.
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
Restore a missing recorded model before resuming rather than substituting another model.

The interface stores the last submitted model and mode and the sidebar setting in the workspace's
`.skyhook/state.json`, beside its sessions, without changing the model configuration.
Session titles are saved with their history.

A sidebar beside the conversation shows the session's title on its first line; click it to rename
the session. Below, it lists the MCP servers the viewed agent can use, with their startup status,
and the viewed agent's todos, with the viewed agent's capabilities pinned to its bottom. For the
root agent of a new session these are what the mode of the next message grants, so cycling modes
changes them. Once the root agent runs, choosing a mode for the next message that changes what it
runs, including its own mode after the configuration redefined it, previews the change: a green
`+` marks a server that mode would add, and a server it would remove is greyed out. A section with
nothing to list is left out, title included. The sidebar is shown by default; `Ctrl+X B` or
`/sidebar` toggles it.
Terminals narrower than 100 columns hide it without changing the saved setting.

The bottom bar uses this format:

```text
default · openai       8.4k · 200k(31.2k) · 42% (54k/128k)
```

Values are session output tokens, session total input tokens (uncached input), and the selected
agent's context occupancy as the last response that reported its prompt counted it
(tokens/model capacity). Session totals include children and compaction; a count a provider did
not report adds nothing. Context includes instructions, tools, history, runtime state and that
response; it is not cumulative usage. It appears as `—` until a response reports its prompt,
including after compaction.

## Keyboard and mouse

The command palette and `/help` also list these shortcuts. `Ctrl+X` is a leader: release it, then press the next key when ready, or `Esc` to cancel.

| Shortcut | Action |
| --- | --- |
| `Ctrl+P`, `/` | Commands |
| `Ctrl+X N`, `Ctrl+X S`, `Ctrl+X W` | New session, switch session, close session |
| `Ctrl+X L`, `/rename` | Rename the session |
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
| `PageUp`, `PageDown` | Scroll history; page through a draft taller than its box |
| `Ctrl+Alt+U`, `Ctrl+Alt+D` | Half-page scrolling |
| `Home`, `End` in content | Beginning, latest |
| `/`, `n`, `N` in content | Search, next/previous match |
| `[`, `]` in content | Previous/next inspector tab |
| `Esc` | Close local UI, dismiss/cancel prompts, otherwise interrupt work, cancelling the running foreground tool |
| `Ctrl+C` | Clear draft, otherwise interrupt (as `Esc`)/quit; in a prompt, clear its text, otherwise as `Esc` |
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
the selected item. The Agents palette labels its Status column and its Output, Input (uncached),
Context, and Time statistics. The composer
wraps at word boundaries and grows with its draft up to half the space between the top and
bottom bars, then scrolls to follow the cursor; the mouse wheel over a taller draft scrolls it
without moving the cursor. It supports word movement, selection, `Ctrl+A/E`,
`Ctrl+W`, `Ctrl+U/K`, undo with `Ctrl+Z` or `Ctrl+-`, and redo with `Ctrl+Y`, `Ctrl+Shift+Z` or
`Ctrl+.`. `Home`/`End` move to the start/end of the line, and `Ctrl+Home`/`Ctrl+End` to the
start/end of the draft. Up/Down move through displayed input rows; in an empty composer they
recall earlier prompts, and keep stepping through them until the recalled prompt is edited.
Click agent and tool rows, scroll the relevant panel, or drag across text in user/agent messages
and tool output: releasing the mouse copies the selected characters, and `Ctrl+X Y` (or `y` while
content is focused) copies them again. A plain click copies nothing. Selecting text never scrolls the
conversation, and open palettes and text pages stay where they are under the pointer. Selection
supports parts of a line and multiple lines. It stays in place while a reply streams and finishes,
and when the terminal is resized, as long as the selected text is unchanged. Copying preserves
Unicode and the source's own indentation without adding newlines at visual wraps. The indentation
of tool output and arguments, the `+`/`−` markers of changes, and the expand/collapse glyphs and
spinners of blocks and tool cards are not selected. In a table, selection follows the cells in
reading order rather than the drawn grid: each table cell copies as one line, whole however it
wraps, without borders or padding, and an empty cell copies as an empty line. Each copy reports
how many characters it sent to the terminal; the terminal does not confirm that it accepted them.
The workspace path is centered in the top bar. The bottom bar shows the model ID, session ID,
and token statistics. These are plain text; use the terminal emulator’s selection gesture
(usually Shift-drag) and copy shortcut. Copy uses the terminal's OSC 52 clipboard support. `@` or `Ctrl+X F` attaches a workspace file
(press `Esc` to keep a typed `@`): image files attach as images and other files as text.
The picker lists up to 10,000 workspace files by path, leaving out what Git and ripgrep would:
hidden files and directories (including `.skyhook`), paths matched by `.gitignore` (in a Git
repository or not), `.ignore` files, and Git's global and repository excludes. It always skips
`target` and `node_modules` directories, and does not follow symbolic links.
Attachments are listed below the composer, and attached text is sent after the message. Pastes longer than 12 lines appear as inline items at the cursor. Type
before, between, or after multiple paste items; move across, select, delete, and undo them as
single editing units; pasting the same text again right after its item expands it into editable
text. Sending or copying expands their original contents in place. Shorter pastes
remain ordinary editable text. Use `/attachments` to inspect or remove pasted items and attachments.
Questions and permissions open even while inspecting the agent tree or
conversation; open menus and search keep input focus until closed. `Esc` dismisses foreground
questions and permissions without answering them; reopen pending requests with `Ctrl+X R`
(or `/attention`, labelled **Reopen questions and permissions** in the palette).
Submitting a new prompt rejects any pending question batches; those batches can no longer be reopened.
Commands have no terminal, so password prompts from SSH and from `sudo` in commands (locally or
on a target) appear as authentication prompts. The typed answer stays out of the conversation, but
it goes to whichever process asked, and a command the agent runs can ask too and capture it.
As a hint, each prompt names the program that appears to have asked and, when it runs under one,
the job: for example `Requested by /usr/bin/ssh` or `Requested by sudo (running as uid 0) in job 12`.
A program running as another user, such as `sudo`, shows only its name and user, since its
executable cannot be read. This is best-effort attribution, not proof of who asked or where the
answer goes: a command started by a genuine program, such as an SSH `ProxyCommand`, can ask in
that program's name and receive the answer. Answer only prompts you expect from the work you
asked for, and treat one that names a program you don't expect (a script, a shell, Skyhook
itself) or an `unknown program` as raised by the agent's command.
For background questions and authentication prompts, `Esc` cancels the request;
cancelled requests cannot be reopened. Authentication prompts take priority over questions,
permissions, menus, and search; interrupted question drafts resume afterward.

Renaming opens a prompt of its own, ahead of pending questions and permissions, with the title you
set ready to edit, or empty while the title is automatic; the composer and its draft are left
alone. `Enter` saves the title, and saving it empty returns the session to its automatic title.
`Esc` cancels the rename. A draft has nothing to rename until its first message is sent.

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

# Terminal interface

## Conversation and inspector

User messages appear on the right and assistant messages on the left. Tool previews show their
remote execution target after the tool name, such as `exec @lab-monitoring`; local calls omit `@root`. Tool calls expand inline
with named argument fields, nested lists, and syntax-highlighted scripts, commands, file content,
and diffs. Prose arguments, such as agent prompts, wrap at word boundaries; commands and source
retain whitespace-preserving wrapping. JSON results and result pages are pretty-printed; source
and plain-text log whitespace is preserved. Light and dark themes use a consistent text and syntax palette throughout the UI:
blue headings and emphasis, cyan links and targets, and semantic colours for code, tool output,
and agent status. Command palette shortcut hints are muted and right-aligned.
Tool-call headers keep neutral text with blue expand arrows, cyan remote targets, and status-coloured
icons and labels. Failed calls keep their error details inside the expanded Output section,
including failures that occur before a job is created, rather than adding separate JSON messages.
Language-labelled Markdown code fences share the tool-output syntax palette and have a distinct,
darker background sized to their content, with one cell of padding. Wrapped Markdown text retains
list and quote indentation; visual padding is excluded from copied code. Unknown languages and
oversized code retain readable fallback text. The interface keeps its neutral surfaces;
dark mode uses a pure black background. All formatting is local to the UI and leaves session
records unchanged. Expanded sections keep persistent highlights on their first and last content
lines; tool calls also have a connecting gutter beneath the expand arrow. Their bodies retain the
normal background, including on hover. Click an expanded body to
collapse it, or drag to select text. The agent tree appears above the composer while children
are active or a child agent is being viewed, with blank padding matching the input. Click an agent to inspect its conversation
without mixing its output with other agents. Each agent retains its reading position and expanded rows.
Agent tree rows show `@target` for non-root agents. Agent call previews show the child's target,
including while queued or running; an omitted target inherits the calling agent's target.
Agent rows show their own token totals and context usage in the same compact format as the
bottom-right session summary: output · input (uncached) · context.
Status messages, including interruptions and errors, appear as distinct rows in the conversation
log and are saved with the session. They are excluded from the model’s context.
Completed final replies show their recorded model ID in a muted footer below the answer.
The composer always sends to the root agent. While root is busy, Enter queues a follow-up
for its next model request, without interrupting the current request or tools. It does not wait
for the entire turn to finish. `/queue` edits/removes input that has not yet been consumed,
and `/resume` resumes a queue paused by interruption.
`/retry` continues failed or interrupted turns without duplicating their original prompts.
After a session interruption it resumes all interrupted children automatically, regardless of
which agent is selected. Parents with pending waits stay in those waits rather than starting
another model request merely because recovery was requested.

Normal conversation messages, reasoning, tool cards, and successful replies keep their existing
presentation; they do not display attempt counters. Automatic model retries update one error block
for the failed request instead of appending failure rows. Only that block displays the attempt
number, next delay, and a compact diagnostic, including safe HTTP status and error codes
when available. Only the latest failed attempt's partial output is shown in that block. It clears
when the next attempt starts; streaming output and successful answers use the unchanged normal
conversation rendering, without retry labels.
Retries have no fixed attempt limit; older session logs with a recorded limit still display it.
Every attempt start, failure, and scheduled delay remains in the session journal for auditing.
These recovery events are visible to the UI and logs, not added to the model's conversation.

The inspector provides Conversation, Requests, and Jobs tabs. Requests show one metadata row per
logical model request, including compaction requests, with token summaries alongside the row.
Transient retries reuse that request; older logs can contain separate request rows for each attempt.
Full request bodies remain in the session journal for reconstruction but are not rendered in the UI.
Startup warnings appear directly in the conversation log rather than a separate diagnostics page.
Job output is paged and searchable without acknowledging the agent's pending notifications. Select a job
and press `o` for output fields, regex search, and the next page; `c` requests cancellation.
Intermediate child-agent messages and terminal job notifications appear as expandable **Job event**
cards in the conversation. Agent-message cards show the job identity/name, source message sequence,
and readable progress text when expanded, rather than raw runtime envelopes. They retain their
historical payload when the job later completes or is resumed; ordinary user text is not reclassified.
This presentation also applies when reopening older saved sessions.
Remote output is available after transfer completes. Provider-supplied reasoning streams in a separate
expanded block with an animated spinner and collapses as soon as answer text starts (or the response
finishes). Single-line reasoning stays inline without an expand/collapse control and is not
selectable, even when it wraps in a narrow terminal. Reasoning uses the same Markdown rendering as replies. A separate working
spinner appears while a request is active without a reasoning spinner. Click a multi-line block or press Enter when selected to
reopen it, including after resuming a session. `/thinking` toggles expansion of saved reasoning.

## Model selection and UI state

Models are listed in configuration declaration order. For a new session, selection uses
`--model`, then the most recently submitted configured model, then the first model in the list.
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

The interface stores the last submitted model and theme selection in `$XDG_STATE_HOME/skyhook/ui.json`, falling back
to `~/.local/state/skyhook/ui.json`. Writes are atomic and do not rewrite the model configuration.
Session titles are stored separately from conversation history in each session's `ui.json`.

The bottom bar uses this format:

```text
default · openai       8.4k · 200k(31.2k) · 42% (54k/128k)
```

Values are session output tokens, session total input tokens (uncached input), and the selected
agent's estimated current context occupancy (current tokens/model capacity). Session totals
include children and compaction. Context includes instructions, tools, history, and runtime
state; it is not cumulative usage. Missing context data appears as `—`.

## Keyboard and mouse

`Ctrl+X` is a leader: release it, then press the next key within two seconds.

| Shortcut | Action |
| --- | --- |
| `Ctrl+P`, `/` | Commands |
| `Ctrl+X N`, `Ctrl+X L` | New session, session picker |
| `Ctrl+X M`, `/model` | Model for subsequent user messages |
| `Ctrl+X A`, `Ctrl+X I` | Agent picker, focus conversation |
| `/requests`, `/jobs` | Requests, jobs |
| `Ctrl+X ↑`, `Ctrl+X ↓` | Parent, first child |
| `Ctrl+X T` | Dark/light theme |
| `Ctrl+X E` | Edit draft in `$EDITOR` |
| `Ctrl+X Y`, `Ctrl+X X` | Copy message/selection, export conversation |
| `Tab`, `Shift+Tab` | Focus composer, tree, content |
| `Enter` | Send/queue, select, expand |
| `Alt+Enter`, `Ctrl+J`, supported `Shift+Enter` | Newline |
| `PageUp`, `PageDown` | Scroll history |
| `Ctrl+Alt+U`, `Ctrl+Alt+D` | Half-page scrolling |
| `Home`, `End` in content | Beginning, latest |
| `/`, `n`, `N` in content | Search, next/previous match |
| `[`, `]` in content | Previous/next inspector tab |
| `Esc` | Dismiss local interaction or interrupt work |
| `Ctrl+C` | Clear draft, otherwise interrupt/quit |
| `Ctrl+X Q` | Quit |

The command palette omits navigation-only actions; `Ctrl+X I`, `Ctrl+X ↑`, and `Ctrl+X ↓`
remain available to focus the conversation, select the parent, and select the first child.
Menus use arrows, mouse hover, the mouse wheel, or `Ctrl+P/N`; Enter or Tab activates
the selected item. Open palettes isolate hover from the conversation underneath. The Agents
palette labels its Output, Input (uncached), and Context statistics. Theme choices preview
immediately; Escape restores the previous theme and Enter saves the choice. The composer
wraps at word boundaries and supports word movement, selection, `Ctrl+A/E`, `Ctrl+W`,
`Ctrl+U/K`, and undo/redo with `Ctrl+-` / `Ctrl+.`. Up/Down moves through displayed input
rows, reaching prompt history only from the first/last row.
Click agent and tool rows, scroll the relevant panel, or drag across text in user/agent messages
and tool output, then copy the selected characters with `Ctrl+X Y` (or `y` while content is focused).
Selection supports parts of a line and multiple lines; copying preserves Unicode and code indentation
without adding newlines at visual wraps.
The workspace path and session ID in the top bar are plain text; use the terminal emulator’s
selection gesture (usually Shift-drag) and copy shortcut. The bottom bar shows the model ID
and token statistics. Copy uses the terminal's OSC 52 clipboard support. `@` attaches a workspace file; `/attach`
adds an image. Pastes longer than 12 lines and attached file contents appear as inline items
at the cursor. Type before, between, or after multiple paste items; move across, select, delete,
and undo them as single editing units. Sending or copying expands their original contents in
place. Shorter pastes remain ordinary editable text. Use `/attachments` to inspect or remove
paste items and images; `$EDITOR` opens the fully expanded draft for editing.
Questions and permissions open even while inspecting the agent tree or
conversation; open menus and search keep input focus until closed. Dismissed requests can be
reopened with `/attention`. SSH authentication/askpass prompts take priority over questions,
permissions, menus, and search; interrupted question drafts resume afterward. `/diagnostics`
lists startup warnings such as skipped skills.

Within questions and permissions, `↑`/`↓` selects an answer, `PageUp`/`PageDown` scrolls
the prompt text, and `Ctrl+PageUp`/`Ctrl+PageDown` scrolls long answer descriptions.
In multi-question prompts, `←`/`→` switches questions while navigating choices, preserving
each question's selected option and unfinished text. Typing enters text-editing mode, where
`←`/`→` moves the cursor; `Tab` switches between editing and question navigation. `Enter`
confirms an answer and advances to an unanswered question; switching alone never submits.
The mouse wheel scrolls the text or choices beneath the pointer. Drafts remain intact while
answering questions or inspecting details.
Question choices are suggestions: you can select one, optionally add a comment, or provide
a free-form answer. A suggestion without a non-whitespace comment returns its label as a string;
with a comment it returns `{"answer": "selected label", "comment": "user text"}`. Free-form
answers remain strings.

Optional settings live in `$XDG_CONFIG_HOME/skyhook/tui.toml` (or `~/.config/skyhook/tui.toml`):

```toml
theme = "dark"

[keybinds]
model = "ctrl+x m"
inspect = "ctrl+x i"
# Disable an action binding with "none". /help lists available actions.
```

See [sessions and context](sessions-and-context.md) for compaction and persistence,
and the [CLI reference](../reference/cli.md) for startup options.

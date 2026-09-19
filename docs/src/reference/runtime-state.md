# Todos and runtime state

## Todo lists

Every agent has an ordered advisory todo list. `todo()` reads the caller's list;
`todo({items:[...]})` replaces the whole list and returns `{updated:true}`; an empty array clears it. Each item has
`text` and a `status` of `pending`, `in_progress`, or `completed`. Multiple items may be in
progress. Unfinished items do not prevent an agent from finishing.

Any agent allowed to delegate can seed a child with `agent.todos`. The task `prompt` remains
required; seed items use the same `{text, status}` format as `todo.items` and appear in the
child's runtime state before its first model request:

```js
const child = await tool.agent({
  prompt: "Implement the requested change and report the validation results.",
  name: "implement-change",
  todos: [
    {text: "Inspect the implementation", status: "in_progress"},
    {text: "Make the change", status: "pending"},
    {text: "Run relevant checks", status: "pending"}
  ],
  bg: true
});
// Yield for an event, then inspect the child; an event need not mean completion.
await tool.wait({timeout:300});
const childStatus = await tool.job(child.id).output();
if (childStatus.state === "queued") return childStatus;
return tool.todo({job: child.id});
```

The child owns subsequent edits. Ancestors can inspect a descendant using its agent job ID,
including after it finishes; `items` and `job` cannot be combined. Inspection is available once
the child has initialized; a queued launch may not have a list yet. Reads return `{items}`;
replacements return `{updated:true}`. An agent without todos has an empty list. Inside the child, progress can be
updated with the same tool:

```js
return tool.todo({items: [
  {text: "Inspect the implementation", status: "completed"},
  {text: "Make the change", status: "in_progress"},
  {text: "Run relevant checks", status: "pending"}
]});
```

Lists persist with the session, including completed and interrupted child lists. Resume preserves
recorded statuses. Host interfaces observe `SessionEvent::TodosReplaced` through the existing
runtime event subscription.
The public `TodoItem`, `TodoStatus`, and `TodoSnapshot` types live in `skyhook::agent`.

## Per-request state snapshot

Unless the model profile's [`state_mode`](../configuration/providers-and-models.md) is `none`,
each model request carries a fresh, compact-text `<skyhook_state>` snapshot: after the history
(`dynamic`, the default) or committed to the conversation after earlier snapshots (`persist`). Its first
line is `date:YYYY-MM-DD`, using the host's current local date, refreshed per request rather
than fixed in the system prompt at agent startup. The optional `jobs:` and `todos:` sections
follow in that order; empty sections are omitted. The format is self-describing; the system
prompt does not explain it. This presentation does not change the JSON returned by job or todo tools.

The jobs section starts with `jobs: job parent tool name state age_s turns tool_calls`.
Each following row contains those fields separated by single spaces. `parent` is `-` for
jobs directly visible to the caller; nested active child-agent rows identify their immediate
parent job explicitly. Ordinary tool jobs are not included as nested children. Tool and name
values are unquoted only when they consist of ASCII letters, digits, `_`, `-`, `.`, or `/`;
other values, including a literal `-`, are JSON-quoted. Missing names or counters use `-`.

`age_s` counts elapsed whole seconds since job creation, including time queued or waiting for
input, clamped to zero if the clock moves before the creation timestamp. Child-agent jobs report
the child's selected target and workspace once initialized. Optional `target="..."` and
`workspace="..."` fields use JSON-quoted values and appear only when they differ from the
snapshot's current execution location. Every row is compared with that location, never with
its parent row; omitted fields do not inherit a parent's overrides. Target fields are omitted
when target capabilities are disabled.

Active agent jobs include exclusive `turns` and `tool_calls` counters. A turn is a complete,
committed assistant response (including responses containing tool calls); failed attempts,
incomplete responses, retries, and compaction requests do not add turns. Tool calls count jobs
launched by that child, including calls inside its scripts, but not work owned by its descendants.
Counters remain cumulative across retained-child follow-ups and session resume. Counters are
per-agent, not subtree totals.

The todos section retains every item, including completed items, in its original order. Each
consecutive run of the same status starts with `pending:`, `in_progress:`, or `completed:`;
each item's text follows on its own line, indented by two spaces and JSON-quoted. A status
heading repeats if that status occurs again after another status; items are not globally regrouped.

```text
<skyhook_state>
date:2026-09-05
jobs: job parent tool name state age_s turns tool_calls
7 - exec run-tests running 12 - -
8 - agent review running 9 2 3 workspace="/home/user/review"
9 8 agent - waiting_input 4 1 0
todos:
completed:
  "Inspect the implementation"
in_progress:
  "Make the change"
pending:
  "Run relevant checks"
</skyhook_state>
```

In this example, job 9 uses the snapshot's current location, not job 8's workspace override.
With no active jobs or todos, only the date line remains inside the state block.

These snapshots are assembled at request time
and never appended to durable conversation history. Actual job notifications, tool exchanges,
and todo replacement events remain durable. Provider caching behavior is
unchanged; transient history does not guarantee exclusion from provider KV caches.

# Todos and runtime state

## Todo lists

Every agent has an ordered advisory todo list. `todo()` reads the caller's list;
`todo({items:[...]})` replaces the whole list and returns `{updated:true}`; an empty array clears it.
Each item has `text` and a `status` of `pending`, `in_progress`, or `completed`. Multiple items may
be in progress. Unfinished items do not prevent an agent from finishing.

Any agent allowed to delegate can seed a child with `agent.todos`. The task `prompt` remains
required; seed items use the same `{text, status}` format as `todo.items` and appear in the
child's runtime state before its first model request:

```js
return tool.agent({
  prompt: "Implement the requested change and report the validation results.",
  name: "implement-change",
  todos: [
    {text: "Inspect the implementation", status: "in_progress"},
    {text: "Make the change", status: "pending"},
    {text: "Run relevant checks", status: "pending"}
  ],
  bg: true
});
```

The child owns subsequent edits. Ancestors can inspect a descendant with `todo({job: childJobId})`,
including after it finishes; `items` and `job` cannot be combined. Inspection is available once
the child has initialized; a queued launch may not have a list yet. Reads return `{items}`.
An agent without todos has an empty list. Inside the child, progress can be updated with the
same tool:

```js
return tool.todo({items: [
  {text: "Inspect the implementation", status: "completed"},
  {text: "Make the change", status: "in_progress"},
  {text: "Run relevant checks", status: "pending"}
]});
```

Lists persist with the session, including completed and interrupted child lists. Resume preserves
recorded statuses. For Rust host observation, see
[embedding](../development/embedding.md#host-observation-api).

## Per-request state snapshot

The model profile's [`state_mode`](../configuration/providers-and-models.md#context-and-output-budgets)
controls whether and how the model receives a fresh `<skyhook_state>` snapshot with each request.

The snapshot shows the host's current local date, active jobs, and the caller's todo list.
Empty job and todo sections are omitted. This is a compact model-facing view, not the JSON
returned by job or todo tools; scripts should use those tools to inspect state.
Dynamic snapshots are still recorded with model requests; this mode is not a data-retention or
provider-cache control.

### Reading job progress

Job rows identify the job, parent, tool, optional name, state, age, and child-agent progress.
Directly visible jobs have `-` as their parent; nested active child agents identify their
immediate parent job. Ordinary tool jobs are not shown as nested children.

`age_s` is elapsed whole seconds since creation, including time queued or waiting for input.
Child-agent jobs report the child's selected target and workspace once initialized. Location
overrides are shown only when they differ from the snapshot's current execution location.
Omitted locations refer to that current location, not to the parent row's overrides. Targets
are omitted when target capabilities are unavailable.

Child-agent `turns` and `tool_calls` counters describe that agent alone, not its subtree:

- A turn is a complete, recorded assistant response, including responses containing tool calls. Failed or
  incomplete attempts, retries, and compaction requests do not add turns.
- Tool calls count jobs launched by that child, including calls inside its scripts, but not work
  owned by its descendants.
- Counters remain cumulative across retained-child follow-ups and session resume.

Todos retain their original order, including completed items. For example:

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

Here, job 9 uses the snapshot's current location, not job 8's workspace override. With no
active jobs or todos, only the date remains.

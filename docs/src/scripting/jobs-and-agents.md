# Jobs and agents

## Delegation and child input

Child agents retain their conversation for follow-up work. Send instructions with
`tool.job(id).send({value: instructions})`; the returned JobView's payload includes
`.result.accepted`. A running child receives the input at its next model-request boundary,
without interrupting its current request or tools. Children do not need `receive()` for these
updates.

A retained child whose job is `completed`, `failed`, or `interrupted` resumes under the same
job and agent identity, including after a session restart. New instructions are appended to its
existing conversation. Explicitly `cancelled` children and non-agent jobs cannot be resumed.
`jobs({job})` then exposes the latest run's saved result, not an archive of earlier results;
the child conversation and messages already delivered to the parent remain intact.

After a session interruption, retry resumes every retained failed/interrupted child without
selecting them individually. A child-only retry leaves a parent waiting on work untouched and
does not add a root model request. Retrying without an instruction continues the same history
without adding a synthetic message.

A background child's visible text replies, including progress and final replies, arrive as
message events without waiting for the child job to finish. Events carry `kind: "message"`,
the child job `id`, source `message` sequence, optional `name`, and `text`. They reach the
parent at a model-request boundary, not by interrupting an in-flight request. Pending messages
survive session restart. Reading a background job's output does not consume its message events.

Message delivery does not mean the child has finished: completion also requires its owned work
and queued inputs to be resolved. A completed background child's notification references its
`meta.last_message` instead of repeating the reply already delivered. Explicit `jobs({job})`
reads and script calls still expose the saved final result; progress text is not concatenated
into it. A foreground child returns its final reply in the call's `.result`, with no progress
or later message events. The parent cannot make another model request until its foreground
calls return.

Child names are scoped to their caller: `A` and `B` may each create a child named `worker`, but
all scripts owned by `A` share `A`'s child-name scope. A child retains its name even after
termination; send follow-ups to its existing job instead of launching a duplicate. A failed
launch that never created a child does not permanently reserve the name.

Each model-facing `ask` contains one `{id, prompt, options?}` question; independent concurrent calls are merged by the runtime. A child
question batch changes its stable agent job to `waiting_input`; answer that job with
`tool.job(id).send({value: answer})` in a script. For a merged batch, use
`tool.job(id).send({value: {question_id: answer, another_id: answer}})`; the keys are the IDs
included in the waiting job's `questions` output. Each answer can be a string (a suggestion
label or free-form text), or `{"answer": "selected label", "comment": "user text"}` for a
suggestion with a non-whitespace comment. A single question returns that value directly;
a merged batch keeps each value under its question ID.
Root-agent questions go to the host interface instead of a parent agent.
`ask` also accepts optional `bg` (boolean, default `false`). With `bg: true`, the call returns
job metadata immediately so the agent can continue independent work. Use `jobs({job})` to
inspect the pending question and eventual answer; the ask job also
accepts `tool.job(id).send({value: answer})`. Omitting `bg` or passing `false` keeps the usual
foreground wait. Multiple outstanding child question batches are combined on the stable agent
job; parents may answer a subset keyed by question ID, and unanswered questions remain pending.
IDs must be unique across that child's outstanding questions. Background questions are still
owned jobs: children must await or cancel them before finishing.

`agent` accepts an optional `depth` delegation budget. It defaults to zero, making the launched
child a leaf. The value must be less than the caller's own available depth; once no depth remains,
`agent` is omitted from both model tools and script bindings. Its optional `workspace` accepts relative or
absolute directories for both local and remote children. Relative overrides resolve against the
workspace selected by the target rules. Children receive a fresh conversation, shared session
instructions and host-owned skills, and their parent's active model, including model switches and
restored session selections. An explicit child `model` overrides the inherited model, and `mode`
runs the child in a mode in place of the caller's capabilities. Both take only entries configured
with a `hint`, and `mode` only
those granting nothing the caller lacks; an input with nothing to offer is absent.
This applies equally to local children, remote children, and deeper descendants. Agents on
the same target share files; a workspace override creates no filesystem isolation. Children must
finish or cancel all owned jobs and descendants before their agent job completes. Only root agents
may leave background services running after answering.

## Job lifecycle and cancellation

Background-capable tools accept an optional `bg` argument. Calls use the [common JobView
contract](../reference/javascript.md#jobview-response-contract); background launches have
`result: null` and `has_result: false`, while loaded literal `null` results have `has_result: true`.
Cancellation returns target-job metadata in `.result`. A child
question has `state: "waiting_input"`; `jobs({job})` returns its stable question IDs and text in
`presentation.question`, regardless of its size.

Jobs normally follow `queued → running → completed`, optionally cycling through
`waiting_input → running`; `failed`, `cancelled`, and `interrupted` are terminal alternatives.
A `jobs({job})` read acknowledges a pending question or terminal result and suppresses duplicate
automatic notification. Explicit reads remain repeatable, and `jobs` itself never waits; see
[waiting for background work](#waiting-for-background-work).
Unanswered questions do not expire. Send answers to the stable child-agent job ID.

Cancellation cascades through descendant jobs and agents and terminates managed command process
groups locally and remotely. It is a request: use `jobs({job})` to confirm termination. Deliberately
detached processes and unreachable remote hosts limit cleanup. Command timeouts are optional;
omission or null means no deadline. Explicit timeouts of 1–3600 seconds terminate execution,
retaining captured output. A nonzero command exit is a normal result; see
[tool result shapes](../reference/job-output.md#tool-result-shapes) for process payloads.

## Job names

`agent`, `exec`, and `fetch` accept an optional `name` describing the work. Names must use
lowercase kebab-case: start with a letter, then use lowercase ASCII letters, digits, and single
hyphens between nonempty words. Examples include `inspect-config`, `run-tests`, and `build-v2`.
Names are descriptive labels and do not replace job IDs. Tool job names need not be unique;
child-agent names must be unique within their caller's scope, as described above.

```js
return tool.exec({command: ["cargo", "test"], name: "run-tests", bg: true});
```

Names appear in active-job state, job envelopes (including notifications and inspection), and
durable job records. They survive session resume. Omitted or null names leave jobs unnamed;
invalid names are rejected before execution. JavaScript builders also support `.name("run-tests")`.

## Waiting for background work

Launch with `bg: true`, do independent work, then yield for events and inspect status.
`wait` resolves when the agent can act: if other foreground work is still running, it keeps
waiting; otherwise any pending event resolves it. Each notification resolves a given caller's
`wait` once, without consuming the notification content. The next model request receives the
accumulated events together. A wait timeout ends the wait, not the job. A child must finish or
cancel all owned work before it can finish; root background services remain managed by the live
session. In headless mode,
shutdown cancels and drains outstanding work, so workflows must await work they need completed.

```js
const job = await tool.exec({command: ["cargo", "test"], name: "run-tests", bg: true});
let status = await tool.jobs({job: job.id});
while (["queued", "running", "waiting_input"].includes(status.state)) {
  await tool.wait({timeout: 300});
  status = await tool.jobs({job: job.id});
}
return status;
```

To request cancellation, use `await tool.job(job.id).cancel()` and inspect the job until its
state is terminal. Wait events can concern other jobs; they do not prove this job finished.
See [saved job output](../reference/job-output.md) for inspection and pagination, and
[runtime state](../reference/runtime-state.md) for todos and active-job snapshots.

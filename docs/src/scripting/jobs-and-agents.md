# Jobs and agents

## Delegation and child input

Child agents retain their conversation for follow-up work. `tool.job(id).send({value:
instructions})` delivers unsolicited input automatically at a running child's next model-request
boundary, without interrupting the current request or tools. A retained child whose job is
`completed`, `failed`, or `interrupted` is restarted under the same job and agent identity; the
new instruction is appended to its retained conversation. Explicitly `cancelled` jobs are not
resumable. After a session interruption, retry resumes every retained failed/interrupted child
(deepest descendants first), without selecting them individually. A child-only retry leaves a
parent that is waiting on work untouched and does not add a root model request. Retrying without
an instruction continues the same history without adding a synthetic user or parent-input
message.
Children do not need `receive()` to read these updates. Every visible child text reply,
including text-only and final replies, is delivered independently through the background-job
event path, without waiting for the child job to finish. `wait` resolves when the agent can act:
while the agent has other foreground work outstanding it keeps waiting, and the next model request
then carries every accumulated event together; otherwise any pending event resolves it. Each
notification resolves a given caller's `wait` once, and `wait` never consumes content. Message events carry
`kind: "message"`, the child job `id`, source `message` sequence, optional `name`, and `text`.
Completion is determined separately by the agent's remaining work and queued inputs, not by
message delivery. A completed child notification references its `last_message` instead of
repeating that reply, as do automatic model tool responses for completed children. Explicit
`job_output` reads and native script/host calls still expose the saved final result.
Progress text is not concatenated into that final result. A foreground call still needs to
return before its parent can make another model request.
Messages and terminal/question notifications share a snapshot → parent-history commit →
acknowledgment boundary. Failed or abandoned preparation leaves notifications pending, and
caller cancellation cannot split a started append/acknowledgment operation. Pending messages
are recovered from committed child history after restart; committed parent notifications
acknowledge each message independently and prevent duplicate delivery. Reading or claiming a
job result does not consume its message events. Legacy message notifications and exact final
replies in completed runtime notifications are recognized on replay. An old output-claim marker
alone is not evidence of message delivery, so such a reply may be delivered again rather than
discarded. Delivery means inclusion in parent history at a request boundary, not interruption
of an in-flight model request.
Sending to a completed child appends
the instructions after its existing
conversation and starts a new request under the same agent and job ID; it does not start
over with fresh history. `job_output(id)` then exposes the latest run's saved result, not an archive
of earlier results; already-committed parent notifications and the child conversation remain intact.
This resumption applies to successfully completed agent jobs retained
in the live runtime, not arbitrary completed tools or jobs restored after a process restart.
Failed, cancelled, and interrupted agents are not resumed by `send`.
Each model-facing `ask` contains one
`{id, prompt, options}` question; independent concurrent calls are merged by the runtime. A child
question batch changes its stable agent job to `waiting_input`; answer that job with
`tool.job(id).send({value: answer})` in a script. For a merged batch, use
`tool.job(id).send({value: {question_id: answer, another_id: answer}})`; the keys are the IDs
included in the waiting job's `questions` output. Each answer can be a string (a suggestion
label or free-form text), or `{"answer": "selected label", "comment": "user text"}` for a
suggestion with a non-whitespace comment. A single question returns that value directly;
a merged batch keeps each value under its question ID.
Root-agent questions still go directly to the host question handler.
`ask` also accepts optional `bg` (boolean, default `false`). With `bg: true`, the call returns
job metadata immediately so the agent can continue independent work. Use `job_output` or
`tool.job(id).output()` to inspect the pending question and eventual answer; the ask job also
accepts `tool.job(id).send({value: answer})`. Omitting `bg` or passing `false` keeps the usual
foreground wait. Multiple outstanding child question batches are combined on the stable agent
job; parents may answer a subset keyed by question ID, and unanswered questions remain pending.
IDs must be unique across that child's outstanding questions. Background questions are still
owned jobs: children must await or cancel them before finishing.

`agent` accepts an optional `depth` delegation budget. It defaults to zero, making the launched
child a leaf. A caller may grant less than its own available depth; once no depth remains, `agent`
is omitted from both model tools and script bindings. Its optional `workspace` accepts relative or
absolute directories for both local and remote children. Relative overrides resolve against the
workspace selected by the target rules. Children receive a fresh conversation, shared harness
instructions and host-owned skills, and their parent's active model, including model switches and
restored session selections. An explicit child `model` overrides the inherited model.
This applies equally to local children, remote children, and deeper descendants. Agents on
the same target share files; a workspace override creates no filesystem isolation. Children must
finish or cancel all owned jobs and descendants before their agent job completes. Only root agents
may leave background services running after answering.

## Job lifecycle and cancellation

Background-capable tools accept an optional `bg` argument. Model-facing calls return a job view
with `id`, `state`, the actual `target` when enabled, and `result`. Workspace is included when it differs from the caller. Listings and notifications also include tool identity and applicable name/parent metadata. Only annotated fields are shortened; `truncated` lists their total line counts and next read positions.
JavaScript foreground calls return the handler's full native result; background launches return
a job reference. A child question has `state: "waiting_input"`; `job_output` returns its stable
question IDs and text in `question`, regardless of its size.

Jobs normally follow `queued → running → completed`, optionally cycling through
`waiting_input → running`; `failed`, `cancelled`, and `interrupted` are terminal alternatives.
`job_output` acknowledges a pending question or terminal result and suppresses duplicate automatic
notification. Explicit reads remain repeatable. Use `wait` to yield for an event or timeout, then inspect output; `job_output` itself never waits.
Unanswered questions do not expire. Send answers to the stable child-agent job ID.

Cancellation cascades through descendant jobs and agents and terminates managed command process
groups locally and remotely. It is a request: use job_output to confirm termination. Deliberately
detached processes and unreachable remote hosts limit cleanup. Command timeouts are optional;
omission or null means no deadline. Explicit timeouts of 1–3600 seconds terminate execution,
retaining captured output. A nonzero command exit is a normal result with `exit_code`.

## Job names

`agent`, `exec`, `shell`, and `fetch` accept an optional `name` describing the work. Names must use
lowercase kebab-case: start with a letter, then use lowercase ASCII letters, digits, and single
hyphens between nonempty words. Examples include `inspect-config`, `run-tests`, and `build-v2`.
Names are descriptive labels, do not need to be unique, and do not replace job IDs.

```js
return tool.exec({argv: ["cargo", "test"], name: "run-tests", bg: true});
```

Names appear in active-job state, job envelopes (including notifications and inspection), and
durable job records. They survive session resume. Omitted or null names leave jobs unnamed;
invalid names are rejected before execution. JavaScript builders also support `.name("run-tests")`.

## Waiting for background work

Launch with `bg: true`, do independent work, then yield for events and inspect status.
A wait timeout ends the wait, not the job. A child must finish or cancel all owned work before
it can finish; root background services remain managed by the live session. In headless mode,
shutdown cancels and drains outstanding work, so workflows must await work they need completed.

```js
const job = await tool.exec({argv: ["cargo", "test"], name: "run-tests", bg: true});
let status = await tool.job(job.id).output();
while (["queued", "running", "waiting_input"].includes(status.state)) {
  await tool.wait({timeout: 300});
  status = await tool.job(job.id).output();
}
return status;
```

To request cancellation, use `await tool.job(job.id).cancel()` and inspect the job until its
state is terminal. Wait events can concern other jobs; they do not prove this job finished.
See [saved job output](../reference/job-output.md) for inspection and pagination, and
[runtime state](../reference/runtime-state.md) for todos and active-job snapshots.

# Jobs and agents

## Delegation and child input

Child agents retain their conversation for follow-up work. Send instructions with
`control({job: id, send: instructions})`, directly or as `tool.control(...)` in a script; the call
completes, without a result, once the input is accepted. A running child receives the input at its next model-request boundary,
without interrupting its current request or tools. Children do not need `receive()` for these
updates.

A retained child whose job is `completed`, `failed`, or `interrupted` resumes under the same
job and agent identity, including after a session restart. New instructions are appended to its
existing conversation. Explicitly `cancelled` children and non-agent jobs cannot be resumed.
`jobs({job})` then exposes the latest run's saved result, not an archive of earlier results;
the child conversation and messages already delivered to the parent remain intact. A final reply
whose completion the parent had yet to receive when the child resumed, as when input races that
reply, arrives as a message event instead, unless a script already received it as the call's
result.

After a session interruption, retry resumes every retained failed/interrupted child without
selecting them individually. A child-only retry leaves a parent waiting on work untouched and
does not add a root model request. Retrying without an instruction continues the same history
without adding a synthetic message.

A child's result is `{messages?, reply}`: `reply` is its final reply. A background child's
earlier visible text replies arrive as message events without waiting for the child job to
finish. Events carry `kind: "message"`, the child job `id`, optional `name`, and `text`.
They reach the parent at a model-request boundary, not by interrupting an in-flight request.
Pending messages survive session restart. Reading a background job's output does not consume
its message events.

Message delivery does not mean the child has finished: completion also requires its owned work
and queued inputs to be resolved. A completed background child's notification carries the final
reply in `.result.reply`; earlier replies are not concatenated into it. A foreground child's
earlier visible replies are not message events: its result saves them in `messages`, oldest
first, each `{text, at}` with `at` the RFC 3339 UTC time it was made. Like any large result, a
long `messages` is shortened in the call's view and listed in `presentation.truncated`; read the
rest with `jobs({job, field: "/result/messages", start, unit: "elements"})`. Replies a session restart cuts off
from a call whose child still ran arrive as message events. `messages` is absent when there are
none. The parent cannot make another model request until its foreground calls return.

When a child asks, the model's own foreground call blocked on it moves to the background, so the
model's turn goes on and it can answer. That call is either its own `agent` call, which returns
the child's job:

```json
{"id": 12, "state": "waiting_input", "presentation": {"notice": "This child agent asked a question, ..."}}
```

or its foreground `script` call, when the script's `tool.agent(...)` waits on the child, which
returns the script's job, `{id, state: "running", presentation: {notice}}`. The script keeps
running, and its completion arrives as a job event. A script's own `tool.agent(...)` call never
moves: in a foreground or background script it keeps waiting until the child finishes and
resolves with the child's result.

The question goes to the child's parent agent, the agent whose model or script made the call,
never the script itself or a further ancestor. It arrives as the same job event a background
child's question produces, after the replies the child made before it, as message events, in the
same delivery: a parent parked in `wait` wakes for it. A child the model's own call let go of is
then a background job: its later replies arrive as message events and its completion as a job
event. A script's `tool.agent(...)` result saves in `messages` only the replies not delivered as
message events. Nothing repeats a reply.

Child names are scoped to their caller: `A` and `B` may each create a child named `worker`, but
all scripts owned by `A` share `A`'s child-name scope. A child retains its name even after
termination; send follow-ups to its existing job instead of launching a duplicate. A launch
that created no child holds no name: of two launches taking one name at once, the first to start
its child keeps it.

Each model-facing `ask` contains one question: `{header, prompt, options?, multiple?}`.
- `header` is a short title, up to 24 characters. It is the question's tab in the terminal and
  the key its answer is filed under, so it must be unique among the questions asked together.
- `prompt` is Markdown.
- Each option is `{label, description, recommended?, preview?}`. Labels must be distinct. A
  question taking one answer may recommend at most one option; one with `multiple: true`
  needs options and may recommend any number. `preview` is Markdown shown beside the focused
  option, for comparing concrete artifacts such as code or config.

Independent concurrent calls are merged by the runtime. A child question batch changes its
stable agent job to `waiting_input`; answer that job with `control({job: id, send: answer})`.
For a merged batch, use
`control({job: id, send: {"Auth method": answer, "Deploy": answer}})`; the keys are the
headers in the waiting job's `questions` output. A lone question also takes its answer keyed
under its header. An answer to a question taking one answer is a string (a suggestion label or
free-form text), or `{"answer": "selected label", "comment": "user text"}`. An answer to a
question with `multiple: true` is a list of labels and free-form text, or
`{"answers": [...], "comment": "user text"}`. An answer of another shape is refused, and nothing
of that reply is delivered. A single question returns its answer directly; a merged batch keeps
each answer under its header.
Root-agent questions go to the host interface instead of a parent agent.
`ask` also accepts optional `bg` (boolean, default `false`). With `bg: true`, the call returns
job metadata immediately so the agent can continue independent work. Use `jobs({job})` to
inspect the pending question and eventual answer; the ask job also
accepts `control({job: id, send: answer})`. Omitting `bg` or passing `false` keeps the usual
foreground wait. Multiple outstanding child question batches are combined on the stable agent
job; parents may answer a subset keyed by header, and unanswered questions remain pending.
Headers must be unique across that child's outstanding questions. Background questions are still
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
contract](../reference/javascript.md#jobview-response-contract); a background launch reports
the job as launched, `running` and without a `result`, even if it has already finished. A loaded
literal `null` result is `result: null`. A child
question has `state: "waiting_input"`; `jobs({job})` returns its questions, headers included, in
`presentation.question`, regardless of its size.

Jobs normally follow `queued → running → completed`, optionally cycling through
`waiting_input → running`; `failed`, `cancelled`, and `interrupted` are terminal alternatives.
A `jobs({job})` read acknowledges the question or result it returned, suppressing duplicate
automatic notification; an outcome that arrives after the read is still notified. Explicit reads
remain repeatable, and `jobs` itself never waits; see
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

## Long foreground calls

A model's foreground call of a tool that accepts `bg` (`exec`, `script`, `fetch`, MCP tools and
`agent`, but never `ask`) can move to the background by itself, so a long call does not hold its
agent indefinitely:

- Once the calls of one model response have run 30 seconds, a pending event moves them: another
  background job of the agent finishing or asking, or input for the agent (your queued message,
  or a parent's follow-up to a child). An event that arrives sooner waits until then; a call
  that ends first returns its result as usual. A child's progress reply is not an event here.
- After 3 minutes the calls move anyway, except a child agent's, which reports through its own
  replies and questions.

Every still-running foreground call of the response moves together. Each returns its job ID in
state `running` with a notice saying why it moved; the job then behaves exactly like one launched
with `bg: true`: `jobs({job})` reads its output so far, and its result arrives as a job event. Calls
inside a script are not moved on their own: the script's call moves as a whole, and the calls it
waits on keep waiting.

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

To request cancellation, use `await tool.control({job: job.id, cancel: true})`, which completes
without a result, and inspect the job until its state is terminal. Wait events can concern other jobs; they do not prove this job finished.
See [saved job output](../reference/job-output.md) for inspection and pagination, and
[runtime state](../reference/runtime-state.md) for todos and active-job snapshots.

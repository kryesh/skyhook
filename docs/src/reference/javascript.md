# JavaScript runtime

## Supported globals and concurrency

The runtime exposes:

- `Date`, `RegExp`, `Map`/`Set`, `Proxy`/`Reflect`, and `BigInt`;
- `ArrayBuffer`, `DataView`, and typed arrays, including `Uint8Array.fromBase64`,
  `.fromHex`, `.toBase64()`, and `.toHex()`;
- `performance.now()` for measuring elapsed milliseconds;
- `await sleep(ms)` for asynchronous waits, resolving to `undefined`. The delay must be a finite,
  nonnegative number of milliseconds within the host timer range; fractional values are accepted.
  Sleeps stop when the script is cancelled, and unawaited sleeps do not keep it alive;
- `new WorkPool(concurrency).map(items, worker)` and `.run([fn1, fn2, ...])` as async iterables
  yielding successful `{index, value}` results in completion order. `run` requires exactly one
  array of functions, each called with no arguments—not variadic arguments. Invalid `run`
  arguments throw before any task starts. Failures thrown by valid tasks are logged and skipped;
  remaining items continue. Early iterator closure stops scheduling and drains running work;
- `await receive()` waits for the next JSON value sent to the script's own job ID with
  `tool.job(scriptJobId).send({value})`; the script must be launched with `bg: true`.
  This is script input, not child-agent input: agents receive owner updates automatically.

For example, pass the task functions to `run` in one array:

```js
const results = [];
for await (const {index, value} of new WorkPool(2).run([
  async () => 1,
  async () => 2,
  async () => 3,
])) {
  results.push({index, value});
}
return results;
```

## Saved output and serialization

Read or search saved command output with `tool.jobs({job: commandJobId, field: "/result/stdout"})`.
A `jobs` read returns the existing JobView for that job; it does not add another wrapper.
Field-selected output is a job view, not a raw string: available text is in
`presentation.preview.lines`, with pagination metadata alongside it. Field, pagination, and
search selections omit image attachments; whole-output reads can attach saved images.

## JobView response contract

Direct model calls and JavaScript tool calls return the same `JobView` envelope:

```text
id?: number
state?: string
result?: JSON
error?: string
meta?: JobMetadata
presentation?: Presentation
```

Tool results omit absent fields and empty lists at every level. JavaScript receives the same
JSON, so an absent field is `undefined`, as `x?: T` means in TypeScript.

A completed call with nothing more to read or resume (no truncation, page, capture, question,
notice, or retained child that input resumes) omits `id` and `state`. `result` is the native tool payload. It is present exactly when the job
has a result, and a loaded literal `null` result stays `result: null`. A tool with nothing to
return, such as `tool.job(id).send`, completes without a result. `meta` is absent on an ordinary
successful foreground call. Background handles, listings, inspections, and failures carry `meta`
with what the caller does not already know: `tool` and `name` except in the response to the
caller's own foreground call, `parent`, `target`, and `workspace` only when they differ from the
calling agent's own, and `code` for a denial. A pre-admission failure lacks `id`, as does a call the runtime could not make
at all: a tool that is no longer available, or a call interrupted while the session was not
running, is answered with this same failure shape.

`presentation` is absent when a successful foreground response has no truncation, page, capture,
question, or notice. When present, it groups `preview`, `truncated`, `captures`, `question`, and
`notice`, each present only when it has content. Read a page as
`r.presentation.preview.lines` and a question as `r.presentation.question`.
Truncation entries keep their source paths rooted at `/result/...`.

JavaScript receives complete data; model views may shorten fields as described in
[saved job output](job-output.md#automatic-previews).
Tool descriptions' `Result` refers to the envelope's `.result`, not a separate wrapper. A successful
`jobs({job})` read of a failed target is still a successful tool invocation returning that failed
`JobView`; `response.unwrap()` checks the observed job state and can therefore throw for that view.

## `response.unwrap()` and native results

Use `response.unwrap()` synchronously when a completed native payload is needed:

```js
const data = (await tool.read({path: "README.md"})).unwrap();
```

It returns `response.result` when the response is completed (an absent `state` means completed),
or `null` when the job has no result, and throws for failed or pending responses; the thrown error has `error.response` containing the envelope and `error.output` equal to its
`.result`, plus `error.job` and `error.code` when the response has an `id` or denial `code`. The method is non-enumerable and runtime-only: `Object.keys`, JSON serialization,
logging, returning, and saving the envelope retain plain JSON. Nested payloads, JSON copies, and
`receive()` values are not decorated, and there is no built-in `tool.unwrap` helper.

`jobs({job})` reads already return views, so inspect their `presentation.preview`,
`presentation.captures`, or pagination rather than unwrapping them. Operational tool failures are
failed views rather than JavaScript throws; programmer, serialization, `receive`, and sleep errors
still throw.

Before returning results, convert `BigInt` values to strings, dates with `.toISOString()`, and
typed arrays with `Array.from(bytes)`, `.toBase64()`, or `.toHex()`. The runtime does not provide
Node.js APIs, `fetch`, `URL`, `TextEncoder`/`TextDecoder`, or `setTimeout`/`setInterval`.

## Script results and failures

Every script result payload is `{value: <JavaScript return>, console?: <captured text>, failure?: <details>}`.
Scripts without a return have `value: null`, silent scripts have no `console`, and `failure` is
absent on success. In a script JobView, the payload is at `/result` and its return is at
`/result/value`; logs are at `/result/console`.

`console.log(...values)` captures space-separated text, formatting objects as JSON. A running
script's captured text can be inspected with
`tool.jobs({job: scriptJobId, field: "/result/console"})`. These inspections read the currently
available output; they do not subscribe to future writes or wait for script completion.

There is no fixed console-capture size cap. Disk capacity and I/O failures still apply.
Automatic previews can truncate displayed text without discarding captured output; use
[paging or search](job-output.md) to inspect more. The **16 MiB limit applies to JavaScript
source**, not console capture.

On a script execution failure, the saved script payload is `{value: null, console?: <captured text>, failure?: <details>}`;
the enclosing JobView is `failed` and carries its `error`: the thrown error's message and stack,
or the JSON of any other thrown value. Console text belongs to the script result, not job
metadata.

`failure` holds a thrown error's other properties: its `cause` and enumerable properties, such as
the `job` and `code` of an unwrapped failed response, whose full view `tool.jobs({job})` reads.
It is absent when there are none, so use the enclosing JobView's `state` and `error` to determine
whether execution failed. Automatic previews can shorten it.

A top-level `undefined` becomes JSON `null`; nested `undefined` is not coerced or removed and
causes serialization failure.

## Builder execution and policy

Builder setters and object arguments accept the same inputs; omitted values receive the tool's
normal defaults. Use `.set(key, value)` to select an argument by name, for example
`tool.read().set("path", "README.md")`. Unknown `.set` keys throw a `TypeError`; calling a
nonexistent fluent setter produces the usual JavaScript method-call error.

Awaiting a builder executes it immediately. Returning builders recursively executes
independent branches concurrently. Calls use the same capabilities, approvals, path access checks,
and saved-output behavior as direct tool calls. Scripts cannot invoke the `script` tool recursively.

Failed tools retain any partial output (including captured process output on timeout) in the failed
JobView's `.result`; JavaScript operational failures do not reject the builder promise. Use
`response.unwrap()` when failures should throw rather than be handled as data. Programmer and
serialization errors are not operational tool failures and still reject/throw.

See [scripting introduction](../scripting/introduction.md) for lazy-builder examples and
[jobs and agents](../scripting/jobs-and-agents.md) for background work and child input.

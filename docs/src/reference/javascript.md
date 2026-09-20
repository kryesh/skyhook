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

Read or search saved command output with `tool.job(commandJobId).output({field:"/result/stdout"})`.
`job.output()` returns the existing JobView for that job; it does not add another wrapper.
Field-selected output is a job view, not a raw string: available text is in
`presentation.preview.lines`, with pagination metadata alongside it. Field, pagination, and
search selections omit image attachments; whole-output reads can attach saved images.

## JobView response contract

Direct model calls and JavaScript tool calls return the same seven-key `JobView` envelope. Its
required keys and types are:

```text
id: number|null
state: string
has_result: boolean
result: JSON
error: string|null
meta: null|JobMetadata
presentation: null|Presentation
```

`result` is the native tool payload (and may be a loaded literal `null`); `has_result` tells
whether that payload is available. `meta` is `null` on an ordinary successful foreground call
when no full metadata is needed. Background, status, inspection, and failure responses carry
`meta` when available. It has nullable `parent`, `tool`, `name`, `target`, `workspace`,
`last_message`, `code`, and `executed` fields. `target` is `null` when target capabilities are
unavailable. A pre-admission failure can have `id: null`.

`presentation` is `null` when a successful foreground response has no truncation, page, capture,
question, or notice. When present, it groups `preview`, `truncated`, `captures`, `question`, and
`notice`; empty or absent members use their nullable/empty defaults. Read a page as
`r.presentation.preview.lines` and a question as `r.presentation.question`.
Truncation entries keep their source paths rooted at `/result/...`.

JavaScript receives complete data; model views may shorten fields as described in
[saved job output](job-output.md#automatic-previews).
Tool descriptions' `Result` refers to the envelope's `.result`, not a separate wrapper. A successful
`job_output` call on a failed target is still a successful tool invocation returning that failed
`JobView`; `response.unwrap()` checks the observed job state and can therefore throw for that view.

## `response.unwrap()` and native results

Use `response.unwrap()` synchronously when a completed native payload is needed:

```js
const data = (await tool.read({path: "README.md"})).unwrap();
```

It returns `response.result` only when `state: "completed"` and `has_result: true`, including a
completed literal `null`. It throws for failed, pending, or result-unavailable responses; the
thrown error has `error.response` containing the envelope and `error.output` equal to its
`.result`. The method is non-enumerable and runtime-only: `Object.keys`, JSON serialization,
logging, returning, and saving the envelope retain plain JSON. Nested payloads, JSON copies, and
`receive()` values are not decorated, and there is no built-in `tool.unwrap` helper.

`job.output()` and output selections already return views, so inspect their `presentation.preview`,
`presentation.captures`, or pagination rather than unwrapping them. Operational tool failures are
failed views rather than JavaScript throws; programmer, serialization, `receive`, and sleep errors
still throw.

Before returning results, convert `BigInt` values to strings, dates with `.toISOString()`, and
typed arrays with `Array.from(bytes)`, `.toBase64()`, or `.toHex()`. The runtime does not provide
Node.js APIs, `fetch`, `URL`, `TextEncoder`/`TextDecoder`, or `setTimeout`/`setInterval`.

## Script results and failures

Every script result payload is always `{value: <JavaScript return>, console: <captured text>, failure: null|JSON}`,
including silent scripts (`console: ""`) and scripts without a return (`value: null`). `failure`
is `null` on success. In a script JobView, the payload is at `/result` and its return is at
`/result/value`; logs are at `/result/console`.

`console.log(...values)` captures space-separated text, formatting objects as JSON. A running
script's captured text can be inspected at `/result/console` with `job_output` or
`tool.job(scriptJobId).output({field: "/result/console"})`. These inspections read the currently
available output; they do not subscribe to future writes or wait for script completion.

There is no fixed console-capture size cap. Disk capacity and I/O failures still apply.
Automatic previews can truncate displayed text without discarding captured output; use
[paging or search](job-output.md) to inspect more. The **16 MiB limit applies to JavaScript
source**, not console capture.

On a script execution failure, the saved script payload is `{value: null, console: <captured text>, failure: <details>}`;
the enclosing JobView is `failed` and also carries its `error`. Console text belongs to the script
result, not job metadata. A top-level `undefined` becomes JSON
`null`; nested `undefined` is not coerced or removed and causes serialization failure.

## Builder execution and policy

Builder setters and object arguments accept the same inputs; omitted values receive the tool's
normal defaults. Awaiting a builder executes it immediately. Returning builders recursively executes
independent branches concurrently. Calls use the same capabilities, approvals, path access checks,
and saved-output behavior as direct tool calls. Scripts cannot invoke the `script` tool recursively.

Failed tools retain any partial output (including captured process output on timeout) in the failed
JobView's `.result`; JavaScript operational failures do not reject the builder promise. Use
`response.unwrap()` when failures should throw rather than be handled as data. Programmer and
serialization errors are not operational tool failures and still reject/throw.

See [scripting introduction](../scripting/introduction.md) for lazy-builder examples and
[jobs and agents](../scripting/jobs-and-agents.md) for background work and child input.

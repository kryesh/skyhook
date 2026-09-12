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
Field-selected output is a job view, not a raw string: available text is in `preview.lines`,
with pagination metadata alongside it. Field, pagination, and search selections omit image
attachments; whole-output reads can attach saved images.

Before returning results, convert `BigInt` values to strings, dates with `.toISOString()`, and
typed arrays with `Array.from(bytes)`, `.toBase64()`, or `.toHex()`. The runtime does not provide
Node.js APIs, `fetch`, `URL`, `TextEncoder`/`TextDecoder`, or `setTimeout`/`setInterval`.

## Script results and failures

Every script result is `{value: <JavaScript return>, console: <captured text>}`, including
silent scripts (`console: ""`) and scripts without a return (`value: null`). This wrapper applies
to both public job views and native/programmatic script results; only script results need this
extra `.value` unwrapping. Ordinary tool results inside JavaScript remain unchanged. In a script
job view, the return is at `/result/value` and logs are at `/result/console`.

`console.log(...values)` captures space-separated text, formatting objects as JSON. Console
capture is disk-backed and each log write is flushed, so a running script's captured text can
be inspected at `/result/console` with `job_output` or
`tool.job(scriptJobId).output({field: "/result/console"})`. These inspections read the currently
available output; they do not subscribe to future writes or wait for script completion.

There is no fixed console-capture size cap. Disk capacity and I/O failures still apply.
Automatic previews can truncate displayed text without discarding captured output; use
[paging or search](job-output.md) to inspect more. The **16 MiB limit applies to JavaScript
source**, not console capture.

On failure, the result is `{value: null, console: <captured text>, failure: <details>}` alongside
the job error. Console text belongs to the script result, not generic job metadata or a separate
tool-result text block.

## Builder execution and policy

Builder setters and object arguments come from the same strict JSON schema; omitted values receive
the handler's normal defaults. Awaiting a builder executes it immediately. Returning builders recursively executes independent
branches concurrently. All executions still pass through the same registry, policy hook, job
supervisor, persistence, and path authorization checks as model-originated calls. Top-level
`undefined` returns JSON `null`; nested `undefined` values are rejected with their result path.
The `script` tool is deliberately omitted from the runtime, preventing recursive script invocation.

Failed tools retain any partial output (including captured process output on timeout). Model tool
errors include it in an `output` field; JavaScript callers can catch the error and read `error.output`.

See [scripting introduction](../scripting/introduction.md) for lazy-builder examples and
[jobs and agents](../scripting/jobs-and-agents.md) for background work and child input.

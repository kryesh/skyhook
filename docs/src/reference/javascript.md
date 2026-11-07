# JavaScript runtime

## Supported globals and concurrency

Scripts run in V8 with the standard ECMAScript built-ins, including `Intl`, `Temporal` and
`Uint8Array.fromBase64`, `.fromHex`, `.toBase64()`, and `.toHex()`. `WebAssembly` and
`Atomics.waitAsync` are not available. Scripts compute in UTC whatever the host's time zone, so
local-time `Date` methods and `Intl` and `Temporal` defaults use UTC; an explicit `timeZone`
still applies. The runtime adds:

- `performance.now()` for measuring elapsed milliseconds;
- `queueMicrotask(callback)`, which ignores the callback's return value;
- `await sleep(ms)` for asynchronous waits, resolving to `undefined`. The delay must be a number
  of milliseconds from 0 to 1800000 (30 minutes); fractional values are accepted, and any other
  value throws a `TypeError` at the call.
  Sleeps stop when the script is cancelled, and unawaited sleeps do not keep it alive;
- `setTimeout`, `setInterval`, `clearTimeout` and `clearInterval`, passing extra arguments to the
  callback. A missing, negative or `NaN` delay is 0; a delay above 30 minutes throws the same
  `TypeError` as `sleep` rather than firing early. Timers due at the same time run in the order
  they were set. Pending timers do not keep the script alive. An exception a timer or
  `queueMicrotask` callback throws, or a rejection of the promise an async one returns, is
  written to the console as `Uncaught …` without ending the script;
- `TextEncoder` and `TextDecoder`, for UTF-8 only: encoding writes an unpaired surrogate as U+FFFD,
  and a decoder for any other label throws a `RangeError`. `{fatal: true}` makes malformed input a
  `TypeError`, and `ignoreBOM` keeps a leading byte-order mark; `{stream: true}` is not supported;
- `atob` and `btoa`, which throw an error named `InvalidCharacterError` for input that is not
  base64, or for characters above U+00FF, and a `TypeError` without an argument;
- `structuredClone(value)` for values V8 can serialize, cycles included, such as `Map`, `Set`,
  `Date`, `RegExp`, errors, `ArrayBuffer` and typed arrays. Functions, symbols, the runtime's own
  objects (`URL`, `URLSearchParams`, `TextEncoder`, `TextDecoder` and `WorkPool`) and other values
  it cannot clone throw an error named `DataCloneError`, as does a non-empty `transfer` option, and
  a copy whose buffers exceed the ArrayBuffer storage limit throws a `RangeError`;
- `URL`, with `URL.canParse` and `URL.parse`, and `URLSearchParams`, following the WHATWG URL
  standard: an invalid URL throws a `TypeError`, as does a missing required argument, and a URL's
  `searchParams` and `search` stay in step;
- `new WorkPool(concurrency).map(items, worker)` and `.run([fn1, fn2, ...])` as async iterables
  yielding `{index, value}` results in completion order, or `{index, error}` with the thrown value for a task that
  threw; remaining items continue. `run` requires exactly one array of functions, each called
  with no arguments—not variadic arguments. Invalid `run` arguments, and a `map` worker that is
  not a function, throw before any task starts. Early iterator closure stops scheduling and
  drains running work;
- `.feed(next, worker, {wake})` for work that becomes ready over time: whenever a slot frees it
  calls `await next(running, free)` (a `Map` of running keys to their items, and how many slots
  are free), starts the returned items whose `key` (a string or number) is not already running,
  and yields `{key, item, value}` or `{key, item, error}` as each completes. Items beyond the free
  slots wait and start, in order, before `next` is called again. It ends when nothing is running
  and `next` starts nothing. An optional `wake` function (such as `receive`) is called while work
  runs and raced alongside it: each value it resolves yields `{wake: value}`. A wake still pending
  when the feed ends is released, so the input it would have taken goes to the next `receive()`;
- `await receive()` waits for the next JSON value sent to the script's own job ID with
  `control({job: scriptJobId, send: value})`; the script must be launched with `bg: true`. Values
  arrive in order. Calling `release()` on a receive you no longer await hands its value, taken or
  not, to the next `receive()`.
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

## Memory limit

Each script call has a **512 MiB JavaScript heap limit** for its objects and strings, and
separately up to 512 MiB of ArrayBuffer and typed-array storage at once. Freed memory restores
capacity; neither is a cumulative allocation, execution-time, or token budget.

An ArrayBuffer or typed-array allocation beyond its limit throws a `RangeError`, which scripts can
catch. Exceeding the heap limit ends the script, which cannot catch it, with the error
"JavaScript heap limit (512 MiB) exceeded",
and the call fails even if the script went on to return. Keep headroom for temporary allocations
during result conversion and serialization.

This is not a total process-memory limit. Rust-side tool results, host bookkeeping, and
other native allocations are outside the budget, and concurrent script calls each have
their own allowance.

## Saved output and serialization

A result field that holds JSON, such as a command's JSON `stdout`, arrives as the decoded value.
`JSON.parse` given an already-decoded object or array returns a copy of it, so parsing such a
field works whether it arrived as text or as the value.

Read or search saved command output with `tool.jobs({job: commandJobId, field: "/result/stdout"})`.
A `jobs` read returns the existing JobView for that job; it does not add another wrapper.
Field-selected output is a job view, not a raw string: the page is in `presentation.preview`,
as `lines` of text, `elements` or `members` of JSON, or query `matches`, with pagination metadata
alongside it. Field, pagination, search, and query selections omit image attachments; whole-output
reads can attach saved images.

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

Tool results omit absent fields and empty lists at every level. Scripts read a field the
response's schema declares but it lacks as `null`: `r.id`, `r.state` and `r.meta` are `null` on an
ordinary completed call, and an absent `stdout` is `null`. The model's views stay sparse. Fields
inside a paged `jobs({job})` result are not filled, since a page may omit them.

A completed call with nothing more to read or resume (no truncation, page, capture, question,
notice, or retained child that input resumes) omits `id` and `state`. `result` is the native tool payload. It is present exactly when the job
has a result, and a loaded literal `null` result stays `result: null`. A tool with nothing to
return, such as `tool.control({job: id, send: value})`, completes without a result. `meta` is absent on an ordinary
successful foreground call. Background handles, listings, inspections, and failures carry `meta`
with what the caller does not already know: `tool` and `name` except in the response to the
caller's own foreground call, `parent`, `target`, and `workspace` only when they differ from the
calling agent's own, and `code` for a denial. A pre-admission failure lacks `id`, as does a call the runtime could not make
at all: a tool that is no longer available, or a call interrupted while the session was not
running, is answered with this same failure shape.

`presentation` is absent when a successful foreground response has no truncation, page, capture,
question, or notice. When present, it groups `preview`, `shape`, `truncated`, `captures`,
`question`, and `notice`, each present only when it has content, and precedes `result`. Read a
text page as `r.presentation.preview.lines` and a question as `r.presentation.question`,
`{questions}`. Presentation never shortens a question. A script never sees a question its
`tool.agent(...)` child asks: the call keeps waiting for the child's result while the question
reaches the script's agent as a job event, and a foreground script's own call moves to the
background so that agent can answer (see
[delegation](../scripting/jobs-and-agents.md#delegation-and-child-input)).
Truncation entries keep their source paths rooted at `/result/...`.

JavaScript receives complete data; model views may shorten fields as described in
[saved job output](job-output.md#automatic-previews). A returned tool response keeps its own
fields whole in the script's own preview, and its result is previewed as a direct read of that
result would be; a page a `jobs` read returns is a presentation already and stays whole.
A value taken out of a response, such as `response.result`, is shortened like any other returned
value.
Tool descriptions give a call's response as `JobView<T>`: the envelope, with the native result
`T` at `.result`. A successful
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

`jobs({job})` reads already return views, so read their `result`, `presentation.preview`,
`presentation.captures`, or pagination rather than unwrapping them. A script's whole-output
`jobs({job})` read holds the target's complete result, never a preview: previews apply only to
what reaches the model, such as the script's own returned value. Operational tool failures are
failed views rather than JavaScript throws; programmer, serialization, `receive`, and sleep errors
still throw.

The runtime does not provide Node.js APIs or `fetch`. Scripts cannot import modules, including
the runtime's own.

## Script results and failures

Every script result payload is `{value: <JavaScript return>, console?: <captured text>, failure?: <details>}`.
Scripts without a return have `value: null`, silent scripts have no `console`, and `failure` is
absent unless a failure has details. In a script JobView, the payload is at `/result` and its return is at
`/result/value`; logs are at `/result/console`.

`console.log(...values)` captures space-separated text, formatting objects as JSON. A running
script's captured text can be inspected with
`tool.jobs({job: scriptJobId, field: "/result/console"})`. These inspections read the currently
available output; they do not subscribe to future writes or wait for script completion.

There is no fixed console-capture size cap. Disk capacity and I/O failures still apply.
Automatic previews can shorten displayed output without discarding captured output; use
[paging, search, or queries](job-output.md#paging-searching-and-querying) to inspect more. The **16 MiB limit applies to JavaScript
source**, not console capture.

On a script execution failure, the saved script payload is `{value, console?: <captured text>, failure?: <details>}`,
where `value` is `null` unless only the returned value's conversion failed;
the enclosing JobView is `failed` and carries its `error`: the thrown error's message and stack,
or the JSON of any other thrown value. The message leads with the error's name unless it is
plain `Error`. Stack frames are `line L:C` positions in the submitted body's own
lines and columns, or `end of source` for a syntax error where input ended early; frames outside
the body are left out, and the callers an `await` resumed are shown as `at async` frames. Console text belongs to the script result, not job metadata.

`failure` holds a thrown error's other properties: its `cause` and enumerable properties, such as
the `job` and `code` of an unwrapped failed response, whose full view `tool.jobs({job})` reads.
It is absent when there are none, so use the enclosing JobView's `state` and `error` to determine
whether execution failed. Automatic previews can shorten it.

Returned values convert to JSON:

- As in `JSON.stringify`, an `undefined` member is omitted, a top-level `undefined`, an
  `undefined` array element or a non-finite number becomes `null`, a `toJSON` method's result
  stands in for its object (a date becomes its ISO string, a `URL` its `href`), and other objects
  become their own enumerable string-keyed properties.
- Where `JSON.stringify` would throw or lose data, a value takes its natural form: a `BigInt`
  becomes its decimal string, a typed array an array of its elements, an `ArrayBuffer`,
  `SharedArrayBuffer` or `DataView` an array of its bytes, a `Set` an array in insertion order, a
  `RegExp` its text such as `"/x+/g"`, and a boxed primitive its value. A `Map` becomes an object
  when its keys are strings or finite numbers that stay distinct as names, and an array of
  `[key, value]` entries otherwise.
- Promises, builders and other thenables anywhere in the value are awaited.
- An `Error` becomes an object of its `name`, `message`, `cause` and other properties.
- An unpaired surrogate in a string becomes U+FFFD.
- Anything else is a problem at its path, such as `$.items[2]`: functions, symbols, `WeakMap`,
  `WeakSet`, `WeakRef` and `FinalizationRegistry` objects, cycles, unpaired surrogates in keys,
  nesting deeper than 100, and a member whose getter or `toJSON` throws or whose promise rejects,
  reported with its error's message.

Conversion runs after the body returns, so the body's own `try`/`catch` cannot catch its problems.
A value with problems still fails the script, with an error listing them by path, but its saved
`value` keeps everything else, with `null` in place of each problem; captured console text is
kept as on any failure.

Tool arguments convert the same way when the builder is made, so a problem throws at the call:
`undefined` properties are omitted, while `undefined` array elements, promises, builders and
unpaired surrogates are problems too. The converted argument must be an object.

## Builder execution and policy

Builder setters and object arguments accept the same inputs; omitted values receive the tool's
normal defaults. Use `.set(key, value)` to select an argument by name, for example
`tool.read().set("path", "README.md")`. Unknown `.set` keys throw a `TypeError`; calling a
nonexistent fluent setter produces the usual JavaScript method-call error.

Awaiting a builder executes it immediately. Returning builders recursively executes
independent branches concurrently. Only returned values expand: inside the body, `await {a: builder}`
awaits the object, not its members, so await each builder or use `Promise.all`. Calls use the same capabilities, approvals, path access checks,
and saved-output behavior as direct tool calls. Scripts cannot invoke the `script` tool recursively.

Failed tools retain any partial output (including captured process output on timeout) in the failed
JobView's `.result`; JavaScript operational failures do not reject the builder promise. Use
`response.unwrap()` when failures should throw rather than be handled as data. Programmer and
serialization errors are not operational tool failures and still reject/throw.

See [scripting introduction](../scripting/introduction.md) for lazy-builder examples and
[jobs and agents](../scripting/jobs-and-agents.md) for background work and child input.

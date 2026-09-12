# HTTP requests with `fetch`

`fetch` is a reqwest-backed HTTP tool, available directly and as `tool.fetch(...)` in scripts.
It runs on the selected execution target: DNS, TLS, proxy discovery, uploads, and downloads all
happen there. Relative paths use that target's workspace; no files are implicitly copied between
machines. Like `exec`, it supports `name`, `bg`, cancellation, and saved job output.

```js
// Read an article without sending HTML boilerplate to the model.
const page = await tool.fetch({url: "https://example.com/article", text: true});

// Send JSON to an API. HTTP 4xx/5xx are responses, not tool failures.
const response = await tool.fetch({
  url: "https://api.example.com/items",
  method: "POST",
  body: {kind: "json", value: {name: "example"}}
});

// Duplicate query parameters and request headers are supported.
const search = await tool.fetch({
  url: "https://api.example.com/search",
  query: [["tag", "rust"], ["tag", "http"]],
  headers: {Accept: "application/json"}
});

// Send a file as the raw request body, without base64-encoding it in model context.
const upload = await tool.fetch({
  url: "https://api.example.com/upload",
  method: "PUT",
  headers: {"Content-Type": "application/gzip"},
  body: {kind: "file", path: "dist/archive.tar.gz"}
});

// Save a download on the execution target. Existing files are preserved unless overwrite:true.
const download = await tool.fetch({
  url: "https://example.com/archive.tar.gz",
  save_to: "archive.tar.gz",
  max_bytes: 104857600,
  timeout: 300
});

// Explicitly opt out of certificate validation for a development HTTPS server.
// This permits untrusted certificates and enables interception; never use casually.
const development = await tool.fetch({url: "https://localhost:8443/health", insecure: true});
```

## Request options

Only `url` is required. `method` defaults to `GET` and accepts standard methods and valid custom
HTTP method tokens. `query` is an ordered array of string pairs, appended to any existing URL query.
The default `User-Agent` is `Skyhook/<version>`; an explicit `User-Agent` header overrides it.
Header values can be strings or arrays of strings. `auth` accepts `{kind:"bearer",token:"..."}` or
`{kind:"basic",username:"...",password:"..."}`; arbitrary authentication schemes can use headers.
Do not combine `auth` with an `Authorization` header or embed credentials in URLs.

`body` has exactly one tagged source:

- `{kind:"text",value:"..."}` for UTF-8 text.
- `{kind:"json",value:...}` for any JSON value, including `null`.
- `{kind:"form",fields:[["key","value"],...]}` for URL-encoded forms with repeated keys.
- `{kind:"base64",value:"..."}` for inline binary data.
- `{kind:"file",path:"..."}` for a regular-file upload as the raw request body.

Multipart form-data encoding is not supported. File bodies can set their media type through the
`Content-Type` request header; they are not interchangeable with multipart uploads.

## Responses and extraction

Responses include final URL/method, status, `ok` (2xx), redirect history, received byte count,
elapsed time, and a tagged `body`: `text`, `base64`, `file`, or `empty`. Response headers are omitted
by default; set the optional `include_headers: true` to return them as a map of repeated values.
`include_headers` defaults to `false` and does not affect the `headers` request-header map.
JSON responses remain decoded text; scripts can use `JSON.parse(response.body.text)`.
`response_format` defaults to `auto` (text for textual content, base64 otherwise); `text` forces
character decoding and `base64` preserves response entity bytes. HTTP decompression is automatic;
these are not raw wire bytes. `save_to` streams to a temporary file and commits on success instead
of embedding the payload. Errors or cancellation do not replace an existing destination.
Automatic job presentation may shorten `body.text` and `body.data`, with continuation markers;
retrieve the complete saved payload using `job_output` fields `/result/body/text` or
`/result/body/data`. Status, opted-in headers, body kind, and other metadata remain intact. JavaScript
calls still receive the complete payload for processing.

`text:true` is separate from `response_format:"text"`: it extracts readable article content from
HTML with **dom_smoothie**, returning plain text and available title/byline/site/language metadata.
It does not execute JavaScript or fetch linked assets. Plain text, JSON, and other textual types
pass through decoded; binary content is not converted. Extraction failures are explicit, never
silently replaced with raw HTML. Fetch with `text:false` to inspect the original response. Empty
responses remain empty. `text:true` cannot be combined with `save_to` or `response_format:"base64"`.
Extraction accepts at most 10 MiB of decoded HTML and 50,000 DOM elements, with bounded parser
concurrency. Character decoding honors BOMs, HTTP charsets, and HTML meta charsets where applicable.

## Limits and failure diagnostics

The default total `timeout` is 30 seconds, `connect_timeout` is 10 seconds, and `max_bytes` is
10 MiB. The response limit can be raised to 100 MiB; total uploads are also capped at 100 MiB.
Timeouts must be between 1 and 3600 seconds and `max_redirects` cannot exceed 20.
Limits apply while streaming, including to decompressed data; exceeding a limit fails
rather than reporting an incomplete body as successful. Model-visible preview truncation is
independent: retrieve saved results with `job_output`. Cancellation and timeouts cannot undo
server-side effects, and requests are not automatically retried.

Transport and processing failures remain failed jobs (and rejected script calls), but include a
structured failure result. It contains `method`, a safe `origin`, `elapsed_ms`, `received_bytes`,
redirect history, and `diagnostic`: `phase`, `error_kind`, and a concise `message`. To keep tool
definitions compact, diagnostic category fields use string schemas rather than exhaustive lists
of labels; the typed runtime classifications and returned values are unchanged. When available,
`diagnostic.os_error` supplies the executing platform, a numeric OS `code`, and a portable `kind`.
Timeouts include `diagnostic.timeout.kind` (`total`, `connect`, or `unknown`) and a `limit_ms` only
when the expiring limit is known. Timing starts inside fetch on the execution target; it does not
include SSH startup or initial tool approval.

Connection refusal, host/network unreachability, DNS failures, TLS failures, typed HTTP proxy
CONNECT failures, and response-processing failures are distinguished when the underlying errors
provide evidence. Otherwise fetch reports a generic transport category; it never infers that a
firewall caused an error. Diagnostic messages do not copy arbitrary error strings, query strings,
credentials, headers, or bodies. Failure URL/redirect context is reduced to origins. Received HTTP
headers are included only with `include_headers: true`; they retain their normal response semantics
and may still contain sensitive response data.
`proxy_origin`, when present, describes an explicit proxy; omission does not rule out an environment
proxy. Use the job's target for source attribution, and interpret OS codes using the reported platform.

If headers arrived before a failure (including an outer timeout), the failure also retains the
HTTP status, `ok`, and byte count, plus headers when `include_headers: true`. Before any response,
those HTTP fields are omitted,
not fabricated. HTTP 4xx/5xx responses still complete normally: a 405 establishes HTTP connectivity,
not successful ingestion. Permission denial and cancellation retain their separate semantics.

In scripts, catch failures inside each worker and inspect `error.output.diagnostic`; merely
logging an Error or allowing `WorkPool` to skip a failed worker loses the structured row:

```js
try {
  const response = await tool.fetch({url, target});
  return {target, http_reached: true, status: response.status};
} catch (error) {
  const failure = error.output ?? {};
  return {
    target,
    http_reached: Number.isInteger(failure.status),
    status: failure.status ?? null,
    elapsed_ms: failure.elapsed_ms ?? null,
    diagnostic: failure.diagnostic ?? null,
    error: error.message,
  };
}
```

The same diagnostic is retrievable from a failed fetch job at `/result/diagnostic`. An uncaught
script failure preserves it under `/result/failure/output/diagnostic` in the script job.

## Redirects, proxies, and security

`redirects` is `safe` by default (follow GET/HEAD), `follow` to follow other methods too, or `manual`
to return the redirect response. `max_redirects` defaults to 5. Changed origins require authorization;
cross-origin requests do not inherit sensitive request headers, and HTTPS-to-HTTP redirects are
rejected. Redirect method rewriting follows HTTP conventions; 307/308 preserve method and body.
`proxy` selects an explicit HTTP proxy; otherwise reqwest uses the target's proxy environment.
`insecure` defaults to **false**. Setting it to **true** disables HTTPS certificate validation for
that invocation only; it does not disable authorization or permit HTTPS downgrade redirects.

All requests require the `network` capability and approval, not just filesystem `read` permission.
File uploads additionally require `read`, and downloads require `write`, including paths inside
the workspace. Loopback and internal-service URLs are supported; this is a general-purpose network
tool, not an isolated browser or a network sandbox. Returned content is untrusted data.
There is no ambient shared cookie jar: set `Cookie` explicitly and use `include_headers: true`
to inspect repeated `Set-Cookie` headers when needed. **Arguments, response bodies, and headers can contain secrets and are subject
to the normal session/job persistence rules**; do not assume HTTP credentials are omitted from
session records or that `insecure` makes authentication safer.

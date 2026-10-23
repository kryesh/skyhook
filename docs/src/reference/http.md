# HTTP requests with `fetch`

`fetch` is an HTTP request tool, available directly and as `tool.fetch(...)` in scripts.
It runs on the selected execution target: DNS, TLS, proxy discovery, uploads, and downloads all
happen there. Relative paths use that target's workspace; no files are implicitly copied between
machines. Like `exec`, it supports `name`, `bg`, cancellation, and saved job output.

```js
// Read a web page as markdown, without its HTML boilerplate.
const page = (await tool.fetch({url: "https://example.com/article"})).unwrap();

// Send JSON to an API. HTTP 4xx/5xx are responses, not tool failures.
const response = (await tool.fetch({
  url: "https://api.example.com/items",
  method: "POST",
  body: {kind: "json", value: {name: "example"}}
})).unwrap();

// Duplicate query parameters and request headers are supported.
const search = (await tool.fetch({
  url: "https://api.example.com/search",
  query: [["tag", "rust"], ["tag", "http"]],
  headers: {Accept: "application/json"}
})).unwrap();

// Send a file as the raw request body, without base64-encoding it in model context.
const upload = (await tool.fetch({
  url: "https://api.example.com/upload",
  method: "PUT",
  headers: {"Content-Type": "application/gzip"},
  body: {kind: "file", path: "dist/archive.tar.gz"}
})).unwrap();

// Save a download on the execution target. Existing files are preserved unless overwrite:true.
const download = (await tool.fetch({
  url: "https://example.com/archive.tar.gz",
  save_to: "archive.tar.gz",
  max_bytes: 104857600,
  timeout: 300
})).unwrap();

// Explicitly opt out of certificate validation for a development HTTPS server.
// This permits untrusted certificates and enables interception; never use casually.
const development = (await tool.fetch({url: "https://localhost:8443/health", insecure: true})).unwrap();
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

Responses include the status, the received byte count (`size`), the elapsed milliseconds
(`duration`), and a tagged `body`: `page`, `unreadable`, `json`, `text`, `base64`, `file`, or
`empty`. Followed redirects add their history (`redirects`), whose last `location` is the final URL.
Response `headers` is absent by default; set `include_headers: true` to return a map of repeated
header values. This option does not affect the `headers` request-header map.

`response_format` chooses how an inline body is returned:

- `auto` (the default): HTML is read as markdown (below). A textual body that is JSON is read as
  that value, following the [JSON output](job-output.md#json-output) rules: the body is then
  `{kind: "json", value}`. A body served as HTML that does not begin like HTML is textual, so JSON
  labelled as HTML is still read as JSON. Other textual bodies are decoded into `body.text`, and
  binary bodies, including any that open with control bytes text never holds whatever their media
  type, are base64 in `body.data`.
- `readable`: like `auto`, but reads any textual body served as HTML as a page, and also reads a
  body as HTML when it looks like HTML but was served as another media type, such as `text/plain`.
- `text`: any body decoded as text exactly as sent, including HTML and JSON.
- `base64`: the response entity bytes.

HTTP decompression is automatic; these are not raw wire bytes. Character decoding honors BOMs, HTTP
charsets, and HTML meta charsets where applicable. Without an `Accept` header, `auto` sends
`text/html,application/xhtml+xml,application/json;q=0.9,*/*;q=0.8`, as a browser asks, so sites
that answer other requests with an empty page or a non-HTML variant return their HTML, while APIs
that serve no HTML still return JSON; `readable` sends
`text/html,application/xhtml+xml;q=0.9,*/*;q=0.8`.
`save_to` saves the response to a file on success instead of embedding the payload, and ignores
`response_format`. Errors or cancellation do not replace an existing destination.

HTML is returned as `{kind: "page", title, byline?, site_name?, language?, scope, markdown}`.
`scope` is `article` when the page has a readable main article, and `markdown` holds only that
article. Otherwise it is `page`, and `markdown` holds the whole page body without scripts,
navigation, form controls, footers, and hidden elements. Markdown keeps headings, absolute links, lists, code
blocks, and tables with a header row, but not images embedded as data; other tables read as their
text. A table cell spanning several columns or rows is repeated in each, so every value stays under
its header.
Extraction does not execute JavaScript or fetch linked assets. HTML that cannot be read is returned
as `{kind: "unreadable", reason, text}` with its decoded source in `text`. `reason` is `too_large`
(over 10 MiB of HTML, 50,000 elements and parse errors, 256 levels of nesting, or 20 MiB built
while converting it), `no_text` (for example a page rendered by JavaScript), or `parser_failed`.
Extraction never fails the call: the status, headers, and redirects are always returned.

Automatic job presentation may shorten a result over its preview budget: the body, and also long
header values and redirect lists, recording what it cut; retrieve the complete saved payload
using `jobs({job})` fields such as `/result/body/markdown`, `/result/body/text`,
`/result/body/value`, `/result/body/data` or `/result/headers`. JavaScript calls still receive the
complete payload for processing.

## Limits and failure diagnostics

The default total `timeout` is 30 seconds, `connect_timeout` is 10 seconds, and `max_bytes` is
10 MiB. The response limit can be raised to 100 MiB; total uploads are also capped at 100 MiB.
Timeouts must be between 1 and 3600 seconds and `max_redirects` cannot exceed 20.
Limits apply while streaming, including to decompressed data; exceeding a limit fails
rather than reporting an incomplete body as successful. Model-visible preview truncation is
independent: retrieve saved results with `jobs({job})`. Cancellation and timeouts cannot undo
server-side effects, and requests are not automatically retried.

Transport and processing failures are ordinary failed jobs with no result: in scripts, a failed
JobView rather than a rejected promise. The error names the operation in progress (for example
`send HTTP GET https://example.com`, `receive HTTP response body`, or the download path), then the
cause: an I/O kind with its OS error code when the failure carries one (such as `connection refused
(OS error 111)`), or a fixed message such as `the TLS handshake failed` or `response exceeds
max_bytes`. A failure after the request was sent notes that the request may have executed; a
`save_to` failure that left its destination untouched also reports that it is unchanged.
Failure messages never copy underlying error text, query strings, credentials, headers, or bodies;
URLs are reduced to their origin. A response's `duration` is measured inside fetch on the execution
target; it does not include SSH startup or initial tool approval.

HTTP 4xx/5xx responses still complete normally: a 405 establishes HTTP connectivity, not
successful ingestion. Permission denial and cancellation retain their separate semantics.

## Redirects, proxies, and security

`redirects` is `safe` by default (follow GET/HEAD), `follow` to follow other methods too, or `manual`
to return the redirect response. `max_redirects` defaults to 5. Changed origins require authorization;
cross-origin requests do not inherit sensitive request headers, and HTTPS-to-HTTP redirects are
rejected. Redirect method rewriting follows HTTP conventions; 307/308 preserve method and body.
`proxy` selects an explicit HTTP proxy; otherwise fetch uses the target's proxy environment.
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

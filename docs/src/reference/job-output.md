# Saved job output

Tools execute once and save their complete results. File reads capture snapshots;
subsequent retrieval does not reread changed files. Search and glob capture their complete result
sets. There is no configured capture-size cap or automatic eviction; storage failures are reported
as failures, with retained partial output marked incomplete.

## Tool result shapes

See the [JavaScript JobView response contract](javascript.md#jobview-response-contract) for the
envelope, metadata, presentation group, and `unwrap()` behavior. This page
focuses on saved output and the task-specific payloads below. A failed or pending view remains an
ordinary response.

Search and glob patterns filter eligible files without overriding hidden-file or ignore settings.
Nulls in tool payloads are preserved; fields a tool has no value for are absent.
Process results contain `exit_code`, or `signal` when a signal killed the process, and the
`stdout` and `stderr` streams that produced output; scripts read an absent stream as `""`.
A `stdout` holding JSON is read as that JSON; see [JSON output](#json-output).
Completed agent calls return their complete answer string without automatic truncation. Child questions return `{questions:[{id,prompt,options?}]}`.

Directory reads return grouped entries with file sizes, for example:
`{kind:"directory",entries:{files:[{name:"main.rs",bytes:4096}],directories:["lib"]}}`.
Groups are `files`, `directories`, `symlinks`, and `other`, with sorted names; empty `symlinks` and
`other` groups are absent, and scripts read them as `[]`.
`read({path:"src",details:true})` returns flat `{name,kind,bytes?}` entries; regular files include sizes in both forms.
Missing paths and operating-system access denials from `read` are successful tool results with
`{kind:"error", error:{code:"not_found"|"permission_denied", message}}`, so workflows can inspect the
error without catching an exception. Tool-policy permission denials remain tool errors.
Search returns `{matches:{"src/main.rs":["12: matching text"]}}`, preserving source whitespace.
`search({pattern:"...",details:true})` returns structured `{path,line,column,text}` matches instead.
An empty search result has `matches: {}`.
`targets({details:true})` returns full target metadata; fields a target does not set are absent.
`target_add` completes without a result.
All defaulted input fields, including `details: false`, are optional in tool schemas.

By default, hidden entries (including `.git`) and ignored files are excluded. `hidden: true`
includes hidden entries, while `no_ignore: true` independently disables ignore files. Searches
rooted in subdirectories inherit ancestor ignore rules; explicitly requested paths remain accessible.

## Diagnostics

Diagnostic messages identify the failed operation and its subject, such as a path, argument,
job, or output field. Workspace-operation failures include the applicable execution target;
host/session failures are not attributed to the caller's remote target. “On session host” is
shown when the viewing agent is on a different target and omitted for an agent on the host.
Inspecting a remote job's saved output does not connect to that machine: a retrieval failure concerns saved output,
while a retrieved execution error retains the original operation's context.

Structured job diagnostics and explicitly registered diagnostic result fields are rendered for the
viewing agent, exposing target aliases only when that viewer's capabilities permit them. This is
not a general redaction guarantee for saved output. A `jobs` output read made with broader
capabilities saves its already-rendered view as ordinary JSON. Later reads of that saved snapshot
with narrower capabilities retain the earlier JSON unchanged, as do reads of script results that
copy it. Arbitrary returned JSON is not inspected for diagnostics or target aliases, and
already-committed model messages are not rewritten.

An error does not by itself establish that an operation had no effects. A command may have
started before output capture failed, a file replacement may have committed before a durability
check failed, and a recursive removal may have deleted some entries. Messages distinguish known
pre-execution failures from known or uncertain later effects. Do not automatically retry a
mutation merely because its result is a failed job.

Outcome distinctions still matter: a nonzero process exit is a completed command result, an
HTTP error status is a normal HTTP result, and a `wait` timeout ends only that wait. Expected
missing/unreadable-file results from `read` remain completed diagnostic payloads. Policy denials,
operating-system access failures, cancellation, and interrupted execution remain distinct.

## JSON output

Process `stdout` and MCP `text` content blocks are checked once when the job finishes, and a
textual HTTP body when it arrives. When the whole text is one JSON object or array, or two or more
objects or arrays in a row (JSON Lines, or concatenated output such as `jq` prints by default),
the value replaces the text; a sequence becomes an array. `stdout` and MCP `text` then hold the
value, and `stdout`'s result type reads `string | object | JSON[]`; an HTTP body becomes
`{kind: "json", value}` (`response_format: "text"` keeps it as text). Scripts and the model both
receive the value, and page or query it as JSON.

The text stays text when it is anything else (a lone number or string, or JSON with other text
around it), is unfinished (cut off or cancelled output), nests deeper than 64 levels, repeats an
object key, or holds a number that double-precision floating point cannot represent as written.
A number qualifies when the shortest decimal that reads back as its nearest double has the same
value as written: `0.1`, `1.0` and `1e30` do; integers above 2^53 such as `9007199254740993`,
over-precise decimals, and out-of-range numbers such as `1e400` do not.

## Automatic previews

JavaScript receives complete result data. The model's automatic view of a finished result is a
preview:

- **Text fields**, strings reached only through objects of at most 100 members, such as `stdout`,
  file `content`, or `body.text`, each keep at most 100 lines or 8 KiB (8192 bytes), whichever
  comes first, counting UTF-8 content bytes before JSON escaping. Together they keep at most
  32 KiB; later strings are shortened like everything else.
- **Everything else** shares one 8 KiB preview budget, which also counts the shape and truncation
  records below. When it is exceeded, strings keep their first 256 bytes; if every element then
  fits, all are kept. Otherwise arrays keep their first elements, with one count shared by every
  array at any depth: the largest that fits, and at most 10. Objects with more than 100 members
  keep their first members in source order the same way; smaller objects keep all of theirs, or
  when that does not fit, none. When even a single sample does not fit, strings get shorter, then
  deeper levels are left empty. Values holding complete fields keep the path to them.

Fields a tool declares complete are never shortened and are not counted: skill instructions,
child agent answers, and the job views `jobs` reads return. Errors and child questions sit
outside `result` and are also complete, including those of tool responses a script returns. A
response can therefore exceed the budget; it does not limit response size.

`presentation` precedes `result`. When arrays or objects were cut, `presentation.shape` describes
the whole result: objects list each member's shape under its own name, and a member only some
objects have includes `absent`, as in `"boolean|absent"`; arrays are `[count, element]`, with `"min..max"` when lengths vary and `[0]` when always empty;
types that vary are joined as `"string|null"`, or `{"|": [...]}` when one is a container; objects
with more than 100 members read `{"*": value, "#": count}`. A shape over half the budget collapses
its deepest levels to `"{…}"` and `[count, "…"]`.

`presentation.truncated` lists each cut in its own unit:

- `{field, total_lines, next_start, next_offset?}` for a string, with the exact position to
  continue from; `next_offset` is present only when that position is inside a line.
- `{field, shown, total_elements, kept?}` for an array.
- `{field, shown, total_members, kept?}` for an object.
- `{field, cuts}` for further cuts within `field`, beyond the first 32 listed.

`kept` lists the index ranges shown, as `[[first, last], ...]`, when a complete field inside an
array or object keeps an element beyond the leading ones; such records are always listed. Reading
a cut array or object continues at its first value not shown. Finished jobs with incomplete captures
include an `Output incomplete.` notice. These rules also apply after session resume and to
completed remote jobs.

## Script result presentation

The enclosing script's `.result` is `{value, console?, failure?}`; silent scripts have no
`console`. Ordinary tools have no console field. Existing JobViews such as
`tool.jobs({job})` reads are not wrapped again.

Presentation never changes the full saved return value. Default script output retrieval returns
the composed script payload; explicit field selections read saved script data. Logging and
returning the same data explicitly produces both outputs.

## Paging, searching, and querying

Explicit `jobs({job})` selections return a view whose `presentation.preview` is one page of the
selected field, in that field's own unit:

| Field | Selectors | Page |
| --- | --- | --- |
| Text: a string, a number, boolean or null, or a capture that is live or unfinished | `start`, `offset`, `limit`, `pattern`, `context` | `lines`, `total_lines`, `next_start`, `next_offset` |
| A JSON object or array | `index`, `limit` | `elements` or `members`, `total_elements` or `total_members`, `next_index` |
| A JSON object or array, queried | `query`, `index`, `limit` | `matches`, `total_matches`, `next_index` |

Using another unit's selectors is rejected with an error naming the field's kind; selectors that
only restate their defaults (`start: 1`, `offset: 0`, `context: 0`, and `index: 0` without
`query`) select nothing, so they are accepted with either kind of field. Job metadata is never truncated.

```js
// Read part of a text field, search it, and continue a long line.
jobs({job:42, field:"/result/stdout", start:300, limit:80})
jobs({job:42, field:"/result/stderr", pattern:"(?i)error|warning", context:2})
jobs({job:42, field:"/result/stdout", start:22, offset:54, limit:100})
// Page a JSON array, then continue at the returned position.
jobs({job:42, field:"/result/stdout/items", index:200, limit:50})
// Select within JSON.
jobs({job:42, field:"/result/stdout", query:"$.items[?@.status.phase == 'Failed'].metadata.name"})
```

`field` is a JSON Pointer: `/result/content` selects a file snapshot, `/result/stdout` and
`/result/stderr` select process streams. For scripts, `/result/console` selects captured console
text and `/result/value` selects the JavaScript return; append pointer segments for nested data,
for example `/result/value/items`. The empty pointer `""` selects the whole saved output, the
members of `{"result": ...}`. `limit` is 1–1000 lines, values, or matches (default 100). Output
inspection returns immediately; it does not wait for new output.

### Text pages

`start` is one-based (default 1); `offset` is a zero-based UTF-8 byte offset within that starting
line (default 0). `context` is 0–20 surrounding lines (default 0), and positive context requires
`pattern`. Pages hold up to 32 KiB of JSON-encoded line content; `field` is omitted when it is the
field the model asked for. `next_start` is absent when no continuation remains, and `next_offset` is
present only when the next position is inside a line. `lines` has one entry per returned line (or
fragment of an oversized line): a string for a plain read, and `{line, text}` with the one-based
source line number for a search. Empty fields have zero lines; a final unterminated line counts,
and a trailing newline does not add an empty line.

Pages prefer whole lines; oversized lines are split at UTF-8 boundaries. The page budget may return
fewer lines than `limit`: use the returned position rather than computing `start + limit`. Offsets
beyond a line or inside a UTF-8 character are rejected. A start past the available lines returns an
empty page with the total. Regex matching is case-sensitive unless inline flags override it.
Matching supports lines up to 4 MiB and reports an explicit resource error for larger lines; ordinary
paging can still read those lines.

For closed fields, a missing `next_start` means no selected content remains. For running fields,
`total_lines` describes currently captured output, and the numeric next position can be retried
after yielding with `wait`, even when no content is currently available. Unavailable output omits
`total_lines`; known empty output retains zero. Repeat the field, regex, and context when continuing
a search; overlapping match context is reconstructed from saved text. Live searches defer incomplete
lines and context windows until more output arrives or capture closes. A wait timeout never stops
the original job.

### JSON pages

`index` is the zero-based first element or member (default 0). A page holds whole consecutive
values in source order within 32 KiB, member names and text included, so it may return fewer than
`limit`: continue at `next_index`, which is absent at the end. A first value that is not whole
within the page is sampled as a preview is, within the page budget, with the page's `shape` and
`truncated` records at that value's own pointers, for scripts and the model alike; select those
pointers to read further into it. A member name longer than the page budget is still shown whole.

### Queries

`query` is a JSONPath (RFC 9535) query over the selected JSON field: `$` is that field. Filters can
use `length()`, `count()`, `value()`, and regular expressions with `match()` (the whole string) and
`search()` (any part). Matches are `{at, value}` in document order, duplicates included, where `at`
is the match's pointer in the saved output, ready to use as `field`. They page by `index` like
elements, with `total_matches` counting all of them. Numeric comparisons use double precision.
When nothing matches, the page carries the queried field's `shape`, which shows the member names
a query can use.

A query reads the selected field's JSON into memory, up to 64 MiB of JSON text; a larger field is
rejected, so query a field within it. This limits a query's input, not its evaluation: once it
starts, a query runs to completion, and queries that repeat selectors can produce many duplicate
matches.

## Persistence and live output

Reads are repeatable and survive session resume. Finished jobs with retained partial output have
an `Output incomplete.` notice, including when viewing the final captured page.

The view's `presentation.captures` lists captures that are not a finished part of the result: live,
unfinished, or abandoned output, even without a structured result:

```json
[{"field":"/result/console","complete":false}]
```

`complete: false` means the capture is still live or incomplete; in particular, a partial JSON capture must not be treated as a valid JSON value.
Select a descriptor's `field` to page or search its retained bytes as text. Capture descriptors survive
failures, cancellation, and session resume even when there is no structured result containing
those fields. Empty or absent result fields are not fabricated to represent partial captures.

Local captures, including script console output, can be read while running. Remote captures
become available as they are transferred; inspecting already-transferred output does not require
reconnecting to the remote machine.

The TUI's automatic output view shows the structured result and previews any available captures
not represented there. This exposes both process streams while live and preserves stderr and exit
status on completion. Explicit field, page, and search selections remain selected; choose
**automatic output** in the Saved output menu to return to automatic viewing. The menu lists up to
100 fields one level below the one shown, from the page of it shown, starting with the result's
fields and captures, and the field above it; **more fields** shows the field from where the
list stops, so the menu lists the rest.

Recorded model requests keep the previews, pages, and notifications the model actually saw;
they are not regenerated from saved output.

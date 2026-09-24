# Saved job output

Tools execute once and save their complete results. File reads capture snapshots;
subsequent retrieval does not reread changed files. Search and glob capture their complete result
sets. There is no configured capture-size cap or automatic eviction; storage failures are reported
as failures, with retained partial output marked incomplete.

## Tool result shapes

See the [JavaScript JobView response contract](javascript.md#jobview-response-contract) for the
seven-key envelope, metadata, presentation group, `has_result`, and `unwrap()` behavior. This page
focuses on saved output and the task-specific payloads below. A failed or pending view remains an
ordinary response.

Search and glob patterns filter eligible files without overriding hidden-file or ignore settings.
Nulls in tool payloads and metadata are preserved; known empty/default output fields do not disappear.
Process results always contain `exit_code: number|null`, `stdout: string`, `stderr: string`, and
`timed_out: boolean`, including empty streams and a false timeout flag. Completed agent calls return
their complete answer string without automatic truncation. Child questions return `{questions:[{id,prompt,options?}]}`.

Directory reads return grouped entries with file sizes, for example:
`{kind:"directory",path:"src",entries:{files:[{name:"main.rs",bytes:4096}],directories:["lib"],symlinks:[],other:[]}}`.
Groups are `files`, `directories`, `symlinks`, and `other`, with sorted names; known empty groups
remain present. `read({path:"src",details:true})` returns flat `{name,kind,bytes?}` entries; regular files include sizes in both forms.
Missing paths and operating-system access denials from `read` are successful tool results with
`{kind:"error", path, error:{code:"not_found"|"permission_denied", message}}`, so workflows can inspect the
error without catching an exception. Tool-policy permission denials remain tool errors.
Search returns `{matches:{"src/main.rs":["12: matching text"]}}`, preserving source whitespace.
`search({pattern:"...",details:true})` returns structured `{path,line,column,text}` matches instead.
An empty search result has `matches: {}`. Grouped maps share a single preview budget.
`targets({details:true})` returns full target metadata. Defaults and `target_add` retain their documented
fields and populate known name/type/host/workspace/via/origin metadata; fields are null only when genuinely unavailable.
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
not a general redaction guarantee for saved output. A `job_output` call made with broader
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

## Automatic previews

JavaScript receives complete result data. Model-facing previews may shorten designated output
fields: file `content`, directory `entries`, process `stdout` and `stderr`, search `matches`, glob
`paths`, skill asset `content` and `assets` trees, HTTP `body.text` and `body.data`, and script
`console` text. Skill instructions, errors, questions, and other fields remain complete; there
is no aggregate response-size limit or whole-result fallback.

Each shortened field independently retains at most 100 lines or 32 KiB (32768 bytes), whichever
is reached first. Strings count UTF-8 content bytes before JSON escaping; arrays and grouped maps
count their saved JSON text and retain only complete items. Grouped maps share one budget across
all groups. Shortened fields keep their original types.

`presentation.truncated` contains `{field, total_lines, next_start, next_offset}` for each shortened
field, reporting its total source lines and exact first unread position. `next_offset` is always
numeric, including `0`. Finished jobs with incomplete captures include an `Output incomplete.`
notice. These rules also apply after session resume and to completed remote jobs.

## Script result presentation

The enclosing script's `.result` is `{value, console, failure}`; silent scripts retain
`console: ""`. Ordinary tools have no console field. Existing JobViews such as
`tool.job(id).output()` are not wrapped again.

Presentation never changes the full saved return value. Default script output retrieval returns
the composed script payload; explicit field selections read saved script data. Logging and
returning the same data explicitly produces both outputs.

## Paging and searching

Explicit `job_output` selections return a view whose `presentation.preview` defaults to
100 lines with up to 32 KiB of JSON-encoded line content. Job metadata is always returned in full.

```js
// Read a selected part of a saved result.
job_output({job:42, field:"/result/stdout", start:300, limit:80})
// Search stored text, with surrounding context.
job_output({job:42, field:"/result/stderr", pattern:"(?i)error|warning", context:2})
// Continue at the returned source line and UTF-8 byte offset.
job_output({job:42, field:"/result/stdout", start:22, offset:54, limit:100})
```

`field` is a JSON Pointer: `/result/content` selects a file snapshot, `/result/stdout` and
`/result/stderr` select process streams. For scripts, `/result/console` selects captured console
text and `/result/value` selects the JavaScript return (append pointer segments for nested data,
for example `/result/value/items`). The empty pointer `""` selects the whole saved output, a
page of `{"result": ...}`. Objects and
arrays have deterministic JSON text views. Long lines are split into UTF-8-safe fragments;
the next position identifies where to continue. Regex matching is case-sensitive unless inline
flags override it. Matching supports lines up to 4 MiB and reports an explicit resource error
for larger lines; ordinary paging can still read those lines.

`start` is one-based (default 1); `offset` is a zero-based UTF-8 byte offset within that
starting line (default 0). `limit` is 1–1000 returned source lines (default 100), including
match context. `context` is 0–20 surrounding lines (default 0); positive context requires `pattern`.
Output inspection returns immediately; it does not wait for new output.

Explicit read pages contain `field` and `lines`, plus `total_lines` (which is `null` when
unavailable) and a next position. `next_start` is `null` when no continuation remains, while
`next_offset` is always present and numeric, including `0`.
`lines` is an array of strings, one per returned line (or fragment of an oversized line),
without per-line objects or match flags. Empty fields have zero lines;
a final unterminated line counts, and a trailing newline does not add an empty line.
Read pages and automatic string previews prefer whole lines; oversized lines are split at
UTF-8 boundaries. The page byte budget may return fewer lines than `limit`: use the returned
position instead of computing `start + limit`. Offsets beyond a line or inside a UTF-8
character are rejected. A start past the available lines returns an empty page with the total.

For closed fields, `next_start: null` means no selected content remains; `next_offset` remains
numeric (usually `0`). For running fields, `total_lines` describes currently captured output and
the numeric next position can be retried after yielding with `wait`, even when no content is
currently available. Unavailable output uses `total_lines: null`; known empty output retains zero.
Repeat the field, regex, and context when continuing a search; overlapping
match context is reconstructed from saved text. Live searches defer incomplete lines and context
windows until more output arrives or capture closes. A wait timeout never stops the original job.

## Persistence and live output

Reads are repeatable and survive session resume. Finished jobs with retained partial output have
an `Output incomplete.` notice, including when viewing the final captured page.

The view's `presentation.captures` lists available captures, even without a structured result:

```json
[{"field":"/result/console","kind":"text","complete":false,"output":null}]
```

`kind` is `text`, `json`, or `unknown`. `complete: false` means the capture is still live or
incomplete; in particular, a partial JSON capture must not be treated as a valid JSON value.
Select a descriptor's `field` to page or search its retained bytes. Capture descriptors survive
failures, cancellation, and session resume even when there is no structured result containing
those fields. Empty or absent result fields are not fabricated to represent partial captures.

Local captures, including script console output, can be read while running. Remote captures
become available as they are transferred; inspecting already-transferred output does not require
reconnecting to the remote machine.

The TUI's automatic output view shows the structured result and previews any available captures
not represented there. This exposes both process streams while live and preserves stderr and exit
status on completion. Explicit field, page, and search selections remain selected; choose
**automatic output** in the Saved output menu to return to automatic viewing. The menu includes
both result fields and available captures.

Recorded model requests keep the previews, pages, and notifications the model actually saw;
they are not regenerated from saved output.

# Saved job output

Tools execute once and capture their complete results to disk. File reads capture snapshots;
subsequent retrieval does not reread changed files. Search and glob capture their complete result
sets. There is no configured capture-size cap or automatic eviction; storage failures are reported
as failures, with retained partial output marked incomplete.

## Tool result shapes

Search and glob patterns filter eligible files without overriding hidden-file or ignore settings.
Tool-owned null metadata is omitted; literal nulls inside file contents, script returns, or user JSON are preserved.
Process results omit empty streams and false timeout flags, keeping exit code zero and nonempty stderr.
Completed agent calls return their complete answer string without automatic truncation. Child questions return `{questions:[{id,prompt,options?}]}`.

Directory reads return grouped entries with file sizes, for example:
`{kind:"directory",path:"src",entries:{files:[{name:"main.rs",bytes:4096}],directories:["lib"]}}`.
Groups are `files`, `directories`, `symlinks`, and `other`, with sorted names and empty groups omitted.
`read({path:"src",details:true})` returns flat `{name,kind,bytes?}` entries; regular files include sizes in both forms.
Missing paths and operating-system access denials from `read` are successful tool results with
`{kind:"error", path, error:{code:"not_found"|"permission_denied", message}}`, so workflows can inspect the
error without catching an exception. Tool-policy permission denials remain tool errors.
Search returns `{matches:{"src/main.rs":["12: matching text"]}}`, preserving source whitespace.
`search({pattern:"...",details:true})` returns structured `{path,line,column,text}` matches instead.
Empty compact directory/search maps are `{}`. Grouped maps share a single normal preview budget.
`targets({details:true})` returns full target metadata without nulls; defaults and `target_add` use compact
name/type/host records with nondefault origin/workspace and configured via where applicable.
All defaulted input fields, including `details: false`, are optional in tool schemas.

By default, hidden entries (including `.git`) and ignored files are excluded. `hidden: true`
includes hidden entries, while `no_ignore: true` independently disables ignore files. Searches
rooted in subdirectories inherit ancestor ignore rules; explicitly requested paths remain accessible.

## Automatic previews

Model-facing direct responses include the job ID, state, applicable target/workspace, and `result`.
Only output fields annotated with `x-skyhook-truncatable: true` may be shortened. Each annotated
field independently retains at most 100 lines or 2 KiB (2048 bytes), whichever is reached first.
Strings count UTF-8 content bytes before JSON escaping; arrays and grouped maps count their saved JSON text and
retain only complete items. Grouped maps share one budget across all groups. All other fields remain intact regardless of size, so there is no
aggregate response-size limit or whole-result fallback.

Annotations cover file `content`, directory `entries`, process `stdout` and `stderr`, search
`matches`, glob `paths`, skill asset `content` and `assets` trees, and script result `console` text. Skill instructions
remain complete. Script results retain `console: ""` even for silent scripts; ordinary tools have
no console field. Shortened fields keep their original types;
`truncated: [{field, total_lines, next_start, next_offset?}]` identifies each one, reports its
total source lines, and supplies the exact first unread position. Finished jobs with an incomplete capture include an `Output incomplete.` notice. Errors
and questions are returned in full. Schemas are persisted with jobs so these rules also apply
after session resume and to completed remote jobs.

## Script result presentation

Full JavaScript tool results remain available for programmatic transformations. When a script
returns an unchanged tool-result object or array, it is presented as that child's native job view,
wherever it appears within the script result's `value` structure. The view replaces the raw tool result and carries
the child job ID, bounded annotated fields, and child read positions. A script still produces
one tool response; custom objects and array ordering are preserved.

Edited tool results remain script-owned data with their original field annotations. Extracted
original arrays and grouped maps retain annotations; extracted primitive strings and newly constructed data do not.
Their unannotated content remains complete. Script-owned truncation markers and console text use
the script job ID; child-view read positions use the child job ID. Presentation never changes the full
saved return value. Default script output retrieval reproduces the composed views; explicit field
selections read the saved script data. Background handles and existing job views are not wrapped
again. Logging and returning the same data explicitly produces both outputs.

## Paging and searching

Script console text uses the shared per-field limit. Explicit `job_output` selections return a `preview`, defaulting to
100 lines with bounded page content. Unannotated job metadata is always returned in full.

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
for example `/result/value/items`). Objects and
arrays have deterministic JSON text views. Long lines are split into UTF-8-safe fragments;
the next position identifies where to continue. Regex matching is case-sensitive unless inline
flags override it. Matching supports lines up to 4 MiB and reports an explicit resource error
for larger lines; ordinary paging can still read those lines.

`start` is one-based (default 1); `offset` is a zero-based UTF-8 byte offset within that
starting line (default 0). `limit` is 1–1000 returned source lines (default 100), including
match context. `context` is 0–20 surrounding lines (default 0); positive context requires `pattern`.
Every argument with a default is optional in the tool schema. Output inspection has no wait argument.

Explicit read pages contain `field` and `lines`, plus `total_lines` when known and a next position when more content may be available.
`lines` is an array of strings, one per returned line (or fragment of an oversized line),
without per-line objects or match flags. Empty fields have zero lines;
a final unterminated line counts, and a trailing newline does not add an empty line.
Read pages and automatic string previews prefer whole lines; oversized lines are split at
UTF-8 boundaries. The page byte budget may return fewer lines than `limit`: use the returned
position instead of computing `start + limit`. Offsets beyond a line or inside a UTF-8
character are rejected. A start past the available lines returns an empty page with the total.

For closed fields, an omitted `next_start` means no selected content remains. Omitted `next_offset` means zero.
For running fields, the total describes currently captured output and the numeric next position
can be retried after yielding with `wait`, even when no content is currently available. Unavailable output omits
`total_lines`; known empty output retains zero. Repeat the field, regex, and context when continuing a search; overlapping
match context is reconstructed from saved text. Live searches defer incomplete lines and context
windows until more output arrives or capture closes. A wait timeout never stops the original job.

## Persistence and live output

Reads are repeatable and survive session resume, without opaque tokens or saved query state.
The old `cursor` argument is no longer accepted. Capture completeness remains internal; finished
jobs with retained partial output have an `Output incomplete.` notice.

Local process output can be read while running. Remote shims capture first and transfer their
results in bounded frames after execution completes. Once transferred, output can be queried
without reconnecting to the remote machine.

The journal stores the exact model-visible previews, pages, and notifications. Full artifacts
are separate; provider-neutral request reconstruction reuses committed content rather than
regenerating it from current files or settings. Session format 2 has no migration layer;
version-1 journals are rejected.

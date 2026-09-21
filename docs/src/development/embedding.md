# Embedding

The `skyhook-agent` package exposes the `skyhook` Rust library crate. Hosts supply configuration,
a workspace, model selection, and interaction handlers rather than depending on terminal UI
state. See [library architecture](architecture.md) for responsibility boundaries and the internal
request lifecycle.

## Constructing a harness and session

The configuration-backed entry point is:

```rust,ignore
let harness = config
    .into_runtime()?
    .select_model(name)?
    .harness_builder(workspace)?
    .build()
    .await?;
let session = harness.new_session().await?;
```

`into_runtime()` admits an immutable configuration generation; `select_model(name)` binds the
host's explicit choice to that generation. Admission and selection precede provider construction,
credential lookup, and workspace access. `harness_builder(workspace)` returns a `HarnessBuilder`
that can be customized before `build().await`; hosts supplying their own providers and model
profiles can instead start with `HarnessBuilder::new(workspace)`.

The configuration-backed builder carries the configured modes and selects the configured default.
`mode(name)` selects another, and `capabilities(set)` limits what modes can grant. A builder without
modes gives the root agent that capability set directly. `PromptOptions::model` and
`PromptOptions::mode` carry selections with a submitted message; omitted selections retain the
active model and mode. A mode change applies to the root agent from the boundary that consumes
it, not retroactively to children it already started. A session pins each mode's definition on
first use and cannot outgrow its original capability ceiling.

Before building, hosts can supply policy, question and sensitive-prompt handlers, additional tools,
and an embedded shim catalog. Core `Config` loading does not load `.env`; environment setup belongs
to the host (the CLI performs its own startup loading).

The builder defaults to `<workspace>/.skyhook/sessions`. `HarnessBuilder::session_root`, including
an override supplied by `Config::session_root`, is passed through unchanged: a relative path is
relative to the process working directory, not the workspace or configuration file. The CLI selects
its own workspace-local session root.

## Host observation API

`SessionHandle::observe().await` returns a snapshot and receiver sharing an atomic revision
boundary. Apply updates to the snapshot in revision order with `ObservationSnapshot::apply`.
It contains:

- Durable records keyed by their original journal sequence.
- Request-scoped live responses keyed by agent and logical request, with assembler snapshots.
- Current agent activity and context-usage estimates.

Live response deltas are provisional, not additional committed messages. A new attempt of the
same logical request resets its live response without removing durable attempt records. On
receiver lag, obtain a fresh observation and replace **both** the snapshot and receiver; continuing
with only one can leave a gap or apply stale live state.

`inspect_jobs`, `inspect_output`, `inspect_output_with_captures`, and `inspect_output_fields` are
host inspection APIs. They do not claim jobs or consume agent notifications. `cancel_job` is the
separate operation that requests cancellation. `startup_warnings`, `warnings`, and `mcp_servers`
expose startup diagnostics and MCP discovery outcomes without inserting diagnostics into model
context or writing directly to a terminal. `record_status` likewise records host-facing status,
not a user message.

`TodoItem`, `TodoStatus`, and `TodoSnapshot` are exported by `skyhook::agent`. Hosts projecting
checklists from observed records apply `SessionEvent::TodosReplaced` and the reconciled todos in
`SessionEvent::Compaction` checkpoints in sequence. Both replace the owning agent's entire list;
child-agent lists remain independent.

## Interruption, continuation, and shutdown

`prompt` and `prompt_with_options` submit input and wait for its turn. `interrupt` stops active
turns while retaining child jobs for continuation; explicit job cancellation is non-resumable.
`continue_turn_with` continues failed or interrupted work without duplicating its input and can
select a model or mode for the continued root turn. Its `ContinueOutcome` distinguishes a root
answer from resumed children and reports whether the selection applied: restarting children does
not imply that a waiting or live root also ran. `continue_turn` uses default options and returns
the root answer, or an empty string when there is none.

Use `Harness::resume_session` to reopen durable state. The runtime reconciles interrupted work
before starting agents and opens fresh provider contexts; it does not reuse old network resources.
Journaled agent settings remain the baseline, subject to restrictions imposed by current host
configuration.

Session databases now use format 10 for structured diagnostics. Format 9 databases are not
migrated and cannot be resumed with this version; retain a compatible Skyhook version to inspect
or resume those sessions, or start a new session.

Await `SessionHandle::shutdown()` before releasing the host's session owner. Shutdown stops runtime
producers, drains accepted job and journal work, and closes MCP and remote resources. The journal
remains available so the host can append a final status after observing shutdown errors; await
that append before dropping the session/store owner.

## Embedded shim catalog

The library's shim catalog is empty by default. Supply `remote::EmbeddedShimCatalog` through
`HarnessBuilder::shim_catalog` to enable remote execution. The CLI supplies its own catalog, but
library hosts do not inherit the CLI's artifacts.

`EmbeddedShimCatalog::from_embedded_assets` accepts names of the form `platform-protocol-arch`
(for example, `linux-ssh-aarch64`) and artifact bytes. Selection uses the destination's protocol,
platform, and architecture. A missing matching artifact produces an explicit deployment error.
See [remote transport and shims](remote-transport-and-shims.md) for build, packaging, and deployment
behavior.

## Reading a session archive

`SessionStore::read_records(root, id).await` returns sequence-ordered records from a read-only
database snapshot without acquiring the session's writer lock. A live owner may continue writing;
archive inspection neither takes ownership nor resumes unfinished work.

The SQLite layout is versioned by `session::SESSION_FORMAT_VERSION`. Opening a database validates
its identity and version; there is no migration or compatibility layer for earlier layouts.

## Reconstructing model calls

The journal records inputs at the shared `Provider` boundary, not backend-specific request bodies
or authentication headers:

- `model_context` stores a `ModelContext`: the purpose, named model-profile snapshot, assembled
  system segments (including harness instructions and location), tool descriptions and schemas,
  and optional response schema. Its `template(agent)` derives the history-free `ModelRequest`,
  including the model ID, reasoning setting, output limit, and agent correlation identity. Context
  records describe request settings, not the lifetime of a `ProviderContext` resource.
- `model_requested` identifies one frozen logical request before its first invocation. It references
  the context record and stores `history` as ordered source-event sequences, `tail` as exact inline
  messages, `history_lifetime`, and purpose. Retries refer back to this request using
  `model_attempt_started`, with separate failure/interruption or completion outcomes. Ordinary
  requests reuse a context record while their settings remain applicable; summarization has a
  separate context with its structured response schema.
- `compaction` stores the exact replacement message, retained original message references, covered
  frontier, previous checkpoint, summary request and attempt, token estimates, schema version,
  and reconciled owner todos. History and todos become active together after persistence.
- Reconstruction resolves those references and inline values. It does not consult current
  configuration, prompt code, live jobs, or the current compaction renderer. Failed and interrupted
  requests keep their recorded inputs; usage identifies the originating request, including summary
  requests and failed attempts.

`session::reconstruct_model_request(&records, sequence)` returns the configured provider name and
reconstructed `ModelRequest` for a `model_requested` sequence, using sequence-ordered records from
`SessionStore`. Attachments and tool images reference content-addressed blobs by SHA-256;
`load_blobs(&mut request).await` on an owning `SessionStore` loads their contents for provider
encoding; unlike `read_records`, opening a store acquires the session's writer lock. Reconstruction
reproduces Skyhook's provider-neutral input, not an API-specific wire encoding or a replay of the
external call.

The `ModelRequest` contract preserves the history/tail split and `history_lifetime` cache hints.
See [history, transient state, and caching](architecture.md#history-transient-state-and-caching)
for their semantics and backend rationale.

Source: [configuration admission](https://github.com/kryesh/skyhook/blob/main/src/config/runtime.rs),
[session handle](https://github.com/kryesh/skyhook/blob/main/src/agent/runtime/session.rs),
[observation](https://github.com/kryesh/skyhook/blob/main/src/agent/observation.rs), and
[request reconstruction](https://github.com/kryesh/skyhook/blob/main/src/session/request.rs).

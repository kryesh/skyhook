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
    .select_model(&name)?
    .harness_builder(workspace)?
    .build()
    .await?;
let session = harness.new_session().await?;
```

`into_runtime()` admits an immutable configuration generation, including any edits the host made
to the `Config`; `select_model(&name)` binds the host's explicit choice, a `ModelRef` parsed from
`provider/model`, to that generation. Admission and selection precede provider construction,
credential lookup, and workspace access. `harness_builder(workspace)` returns a `HarnessBuilder`
that can be customized before `build().await`; hosts supplying their own providers and model
profiles can instead start with `HarnessBuilder::new(workspace)`.

The configuration-backed builder carries the configured modes and selects the configured default.
`mode(name)` selects another, and `capabilities(set)` limits what modes can grant; `build()` rejects
a selected mode the modes do not declare, whichever was set first. A builder without modes gives the
root agent that capability set directly, and no mode applies.
`SessionHandle::selection(model, mode)` issues the `Selection` a submitted message or a continued
turn carries, rejecting a `provider/model` or mode the session does not have; omitted selections
retain the active model and mode. A mode change applies to the root agent from the boundary that
consumes it, not retroactively to children it already started. A session runs the harness's modes
and capability ceiling, whatever it ran under before. A reopened session reports modes the
configuration redefined or removed in `startup_warnings()` and changes nothing while viewed, nor
when calls such as `rename`, `record_status` or `cancel_job` activate it. The next work that runs
the root (a prompt, continue, job delivery or `run_script`) decides its mode once: the selected
one, else its own as configured now, or the default mode once its own is gone (without a default
mode, it keeps running its own edition). A message's
`ModelChanged` and `ModeChanged` are journaled in one transaction with its `MessageCommitted`; a
script's precede the script. Each `ModeChanged` or `AgentStarted` whose definition
differs from that mode's latest journaled one records a new edition, which the journal keeps for
auditing. `SessionHandle::changes_mode(mode)` reports whether the root's next message in `mode`
would change what it runs.

Each `SessionEvent::TitleSet` records its `TitleSource`: `Prompt`, `User` or `Generated`.
`SessionHandle::set_title(title)` gives an automatic title to a session that has neither a title
nor a first prompt to take one from, such as one a script starts, and leaves any other alone; `rename(Some(title))` sets the user's title, and
`rename(None)` journals a `TitleCleared` that returns the session to its automatic title.
`title().await` reads the title the session list shows, as a `SessionTitle` with its text and
`TitleSource`: the user's newest title unless cleared since, otherwise the newest `Prompt` or
`Generated` title, otherwise the first text the user sent the root agent, as a `Prompt` title.

Before building, hosts can supply policy, question and sensitive-prompt handlers, additional tools,
and an embedded shim catalog. A `Policy` answers `Approval::Decided` for what it settles by itself
and `Approval::Ask` for what it asks someone, asking nothing until that future is polled. A call
that may not ask anyone, because a path it names outside the workspace does not exist, drops an
`Ask` unpolled and is refused with `PolicyDecision::refused()`, the reason a person's denial
should carry so the two cannot be told apart. A `SensitivePromptHandler` answers each `SensitivePrompt` with a
`PromptAnswer`: `Secret` for passwords, passphrases and keyboard-interactive prompts, and
`Confirmed` or `Rejected` for the confirmation kinds (`SensitivePromptKind::is_confirmation`), which
cover host keys and agent key use. A handler error means no answer could be obtained; `Rejected` is a
decision, and so is an answer of the wrong shape for the prompt's kind. Core `Config` loading does not load `.env`; environment setup belongs
to the host (the CLI performs its own startup loading).

The builder defaults to `<workspace>/.skyhook/sessions`. `HarnessBuilder::session_root`, including
an override supplied by `Config::session_root`, is passed through unchanged: a relative path is
relative to the process working directory, not the workspace or configuration file. The CLI selects
its own workspace-local session root.

## Host observation API

`SessionHandle::store()` is the session's journal. Its `observe()` returns an `Observation`: the
session's live part, an `ObservationSnapshot`, and a receiver of the updates after it, sharing an
atomic revision boundary. It copies only what the session holds live, so it costs the same however
long the session is, and a host may call it whenever it needs a fresh start. Its `applied()` is
the newest record the session has applied, without observing. The snapshot holds:

- `applied`, the newest record it folds, after which every record arrives as an update, and
  `last_activity`, when the newest record that is the session's activity was journaled.
- `requests`, each agent's newest request, with its lifecycle phase, outcome and usage.
- `jobs`, the jobs the session holds: their owner, call arguments, state, failure, current
  invocation, and when that invocation ran. A job's update arrives just before the record that
  moved it. `RuntimeEvent::JobsUnheld` names the jobs the session let go of, which the snapshot
  drops; a job's last update is its final view until a record moves it again.
- `activity`, each agent's current activity, with when its turn began (or, once it is no longer
  busy, when that turn ended); an agent that stops running, as a parked child does, gets an
  update with none.
- `views`, what the records fold into beside the committed state: `responses`, the live
  responses keyed by agent and logical request (their streamed blocks in arrival order, then the
  authoritative blocks once the response has ended; an agent's responses are its latest turn's,
  and one that settled without committing, failed or interrupted, stays until the agent's next
  agent request, as a reopened session also shows it); `context`, the context gauge, each agent's
  occupancy as its last committed response reported it, cleared by a checkpoint; and `entries`,
  how many entries each agent's transcript holds.
- `usage`, the usage each agent's requests reported.

Apply updates in revision order with `ObservationSnapshot::apply`. Once the session no longer runs
(viewed, failed or closed), the snapshot reads every request, response, job and turn it left going
as interrupted at its last activity. `apply` returns what an update changed beyond what it
carries: `entry`, the ordinal its record took in its agent's transcript (none for a record the
snapshot already folds); `stopped`, the jobs and requests read as interrupted that way, which no
update of their own reports; `requests`, the requests its record changed; `superseded`, the
request of its agent that a newer one replaced, as it left it; `fact`, what the fold decided of
its record that presenting it reads (a `session::Fact`: the request an assistant message
committed under, the message whose call a result answered, or that a failed turn ended on its
request's failure); and `unheld`, the jobs the session let go of, as they left them.

`RuntimeEvent::OutputChanged { job, invocation }` reports that a run captured output after its
output was last read, by any reader; it carries no output. Read it with
`inspect_output_with_captures`. No further one is posted for that run until its output is read
again or the run finishes, whose job update reports the final output.

Live response deltas are provisional, not additional committed messages. A new attempt of the
same logical request resets its live response without removing durable attempt records. On
receiver lag, obtain a fresh observation and replace **both** the snapshot and receiver, then read
again the pages on display; continuing with only one can leave a gap or apply stale live state.

### Reading history in pages

History is read in pages after observing, through the handle's `store()` (a `SessionStore`) and
`jobs()` (a `JobManager`), each in one snapshot of the journal. A `Page` is the first or last of a
list, or the entries after or before a key, which it never includes:

- `transcript(agent, page, limit)` returns consecutive entries of one agent's transcript by
  ordinal, with their `facts`: what presenting them reads beyond them, as the journal leaves it
  (the requests they make or answer, each answer's request and when its turn began, the result
  of each call they made and the calls that started a job, and the failed turns a request's
  failure reports), so a host can show any page on its own. `PageFacts::observe` folds an
  update's record and its `fact` into them.
- `jobs().page(JobScope::Roots(owner) | JobScope::Children(parent), page, limit)` returns an
  owner's jobs that do not nest under a parent (`JobEnvelope::nests`: a job its owner's script
  issued), or a parent's children whoever created them, in id order; `jobs().views(ids)` returns
  jobs by id.
- `requests(agent, page, limit)` returns an agent's requests as the journal leaves them.
- `agents()` returns every agent the session started as the records that say what a host shows
  of it (its start, latest model and mode changes, latest todos and the latest record that ended
  a turn), when the turn that record ended began and ended, when its open turn began, and its
  owner job, read through `through`: a summary's records repeat any update through it.
- `failed_after(agent, record)` says whether an agent journaled a failed turn after a record.

A page sees at least what the observation applied. A host places an update's record at the
ordinal `apply` reports, counted on from the snapshot's `views.entries`: a record at an ordinal a page
holds is one the page already shows, and every later one is news. Requests and jobs read as the
snapshot holds them where it holds them, and as their page read them otherwise; a page in flight
when the session lets a job go may have read it before its final view, which `apply` hands over in
`unheld`, as it hands over a request a newer one replaced in `superseded`.

### Inspection and host records

`inspect_output_with_captures` and `inspect_output_fields` are host inspection APIs. They do not
claim jobs or consume agent notifications. `cancel_job` is the separate operation that requests
cancellation. `startup_warnings`, `warnings`, and `mcp_servers` expose startup diagnostics and MCP
discovery outcomes without inserting diagnostics into model context or writing directly to a
terminal. `record_status` likewise records host-facing status, not a user message.

`TodoItem` and `TodoStatus` are exported by `skyhook::agent`. Hosts projecting
checklists from records apply `SessionEvent::TodosReplaced` and the reconciled todos in
`SessionEvent::Compaction` checkpoints in sequence. Both replace the owning agent's entire list;
child-agent lists remain independent.

## Interruption, continuation, and shutdown

`prompt` and `prompt_with_options` submit input and wait for its turn. `interrupt` stops active
turns while retaining child jobs for continuation, and returns how many turns and unstarted child
agents it stopped. If that is more than none, job deliveries start no turn until
`continue_turn_with` or new input: they stay pending for that request. An interrupt also hands
back input no agent has taken yet: a pending `prompt` returns `Interrupted`, and each
`QueuedPromptReceipt` from `enqueue_prompts` resolves `Interrupted`. Queued prompts are checked
when a request boundary takes them, all of them as one batch. A batch the session refuses fails
the turn, whose `AgentFailed` record says why, and waits with it: its receipts stay pending, each
prompt can still be withdrawn (`QueuedPromptCancellation::is_retained` tells such a prompt) until
`continue_turn_with` takes the batch to try again under its selection, together with anything
queued since. It checks the batch as a request boundary would before it changes anything, and from
then on the root runs it as a turn: a batch those checks or the selection refuse waits again
unchanged, together with anything queued meanwhile, and the call returns why. One that an interrupt
stops first also waits again unchanged, but the interrupt hands back what was queued meanwhile
(`Interrupted`), as it does any input no turn has taken. An interrupt leaves a waiting batch
waiting; a `prompt` hands it back (`Interrupted`), and so does an interrupt that overtakes the
refusal itself, after which the root rests unless a turn is still open. Closing the session drops
it. Explicit job cancellation is non-resumable.
`continue_turn_with` continues failed or interrupted work without duplicating its input and can
select a model or mode for the continued root turn. It always restarts the interrupted or failed
children first; a root turn or batch refused afterwards returns why without undoing that. Its
`ContinueOutcome` distinguishes a root answer from resumed children and reports whether the
selection applied: only a continued root turn applies it, and restarting children does not imply
that a waiting or live root also ran. `continue_turn` uses default options and returns the root
answer, or an empty string when there is none.

Use `Harness::resume_session` to reopen durable state. The session opens to view: it journals
nothing, and its observation reads it stopped at its last activity, until the first call that
changes it, such as a prompt, `continue_turn_with`, a script, a status, a title or a job
cancellation. That call first prepares the root agent; if that fails, for example because its
provider cannot open a conversation, the call fails and the session stays viewed. Otherwise the
runtime reconciles interrupted work in one transaction before starting agents. Shutting down a
viewed session stops nothing and journals nothing: it closes the session, releasing it for another
open, and later calls that would change it are refused. Work the previous process left open is
recorded as ending when that process last did something, so reopening a session is not activity;
record times therefore need not increase with sequence. It opens fresh provider contexts and does
not reuse old network resources.
A root turn that failed or was interrupted before the session closed can still be continued. A root
that closed idle with job deliveries pending resumes interrupted too, so `continue_turn_with` or new
input delivers them.
Journaled agent settings remain the baseline, subject to restrictions imposed by current host
configuration.
A session whose root's recorded model is no longer configured still opens and activates, and
`startup_warnings` says so: calls that only write, such as a title, a status or a job
cancellation, work as usual. Only running the root fails, with `HarnessError::UnknownModel` and
before anything is journaled: a script, or a direct prompt, a continuation or an
`enqueue_prompts` call of which no prompt names a configured model. A selection naming one
replaces the model, as does `SessionHandle::activate_on`, which a viewed session journals in the
commit that activates it. Job deliveries that would wake the root before then begin a turn that
fails as any failed turn does, journaled, and stay pending for the input that names a model. A child whose own model is gone restarts
on the root's, journaled as a model change; with the root's gone too, its restart fails alone.

Session databases use format 26. Earlier formats are not migrated and cannot be resumed with
this version; retain a compatible Skyhook version to inspect or resume those sessions, or start a
new session.

Await `SessionHandle::shutdown()` before releasing the host's session owner. Shutdown stops runtime
producers, drains accepted job and journal work, and closes MCP and remote resources. It interrupts
the session as a crash would: child agents still running end interrupted and can be continued once
the session is resumed, and other running jobs are cancelled. Once it has
begun, new prompts, `continue`, host scripts, new jobs, and input that would restart a finished
job are refused with `SessionError::Refused`; input to a running job and answers to its questions
are still accepted. The
journal remains available so the host can append a final status after observing shutdown errors;
await that append, then `SessionHandle::close()`, which releases the session so it can be opened
again at once rather than once every handle has dropped; closing a closed session does nothing. Closing also prevents retained output
handles from writing to the session; already saved output remains readable. Dropping the last
`SessionHandle` without shutting down closes the session as a crash would leave it and stops its
agents and jobs; resuming it settles what was running. A session closed with nothing journaled, such as one
whose `new_session` caller gave up, is removed.
A write the session journal cannot take stops the session as a crash would: the call that met it,
calls it cut short and every later call return `SessionError::JournalFailed` with the first
cause; `shutdown` and `close` report it too and still release the session. Its observation reads
the phase as failed and otherwise shows what reopening the session would; an observation attached
before then no longer follows it once it folds that phase, and observes again. `SessionHandle::phase` tells whether the session still runs and journals.
A session belongs to the Tokio runtime that created or resumed it: use it only from that runtime,
and keep the runtime running until the session is closed.

## Embedded shim catalog

The library's shim catalog is empty by default. Supply `remote::EmbeddedShimCatalog` through
`HarnessBuilder::shim_catalog` to enable remote execution. The CLI supplies its own catalog, but
library hosts do not inherit the CLI's artifacts.

`EmbeddedShimCatalog::from_embedded_assets` accepts names of the form `platform-protocol-arch`
(for example, `linux-ssh-aarch64`) and a loader that returns a named artifact's bytes, called only
for the artifact being deployed. Selection uses the destination's protocol, platform, and
architecture. A missing matching artifact produces an explicit deployment error.
See [remote transport and shims](remote-transport-and-shims.md) for build, packaging, and deployment
behavior.

## Reading a session archive

`SessionStore::read_records(root, id).await` returns every record, in sequence order, from a
read-only database snapshot without acquiring the session's writer lock: an explicit export of the
whole journal. A live owner may continue writing; archive inspection neither takes ownership nor
resumes unfinished work. `SessionStore::is_open(root, id).await` reports whether a store in any
process holds the session. It takes the session lock shared for an instant; an open racing it
retries once before failing with `AlreadyOpen`. `SessionStore::summary(root, id).await` reads a
session list row without decoding the session, including its title as `SessionStore::title`
reads it, in `last_millis` the time of its newest record that `SessionEvent::is_activity` (closing
a session is not), and, in `stopped`, the failed or interrupted turn that resuming the root agent
would continue, if any. `stopped` reads only the root's journaled turns, so a session that closed
idle with job deliveries pending has none, yet resumes interrupted.
`SessionStore::stats(root, id).await` reads, the same way, the statistics `skyhook stats` reports:
aggregates over the whole journal, its requests read a page at a time, so its memory does not grow
with the session.

The SQLite layout is versioned. Opening a database validates its identity and version; there is no
migration or compatibility layer for earlier layouts.

## Reconstructing model calls

The journal records inputs at the shared `Provider` boundary, not codec-specific request bodies
or authentication headers:

- `model_context` stores a `ModelContext`: the purpose, named model-profile snapshot, assembled
  system segments (including harness instructions and location), tool descriptions and schemas,
  and optional response schema. Its `template()` derives the history-free `ModelRequest`,
  including the model ID, reasoning setting, and output limit. Context records describe request
  settings, not the lifetime of a `ProviderContext` resource.
- `model_requested` identifies one frozen logical request before its first invocation. It references
  the context record and stores the checkpoint it opens with, its history as the agent's commits
  through a message sequence (a summary that overflowed resumes `from` a later one), `tail` as
  exact inline messages, and `history_lifetime`. Retries refer back to this
  request using `model_attempt_started`, with separate failure/interruption or completion outcomes.
  Ordinary requests reuse a context record while their settings remain applicable; summarization
  has a separate context with its structured response schema.
- `compaction` stores the exact replacement message, retained original message references, covered
  frontier, the summary attempt that produced it, the reported occupancy that triggered it (none
  after an overflow), and reconciled owner todos.
  History and todos become active together after persistence.
- Runtime state and job-event content are stored structurally and rendered for the model on
  reconstruction, so a request replays the text the model was sent. A reconstructed context
  carries, before each response since its checkpoint or the agent's latest mode switch, the state
  the request that produced it sent; codecs decide whether to send it.
- Reconstruction resolves those references and inline values. It does not consult current
  configuration, live jobs, or the current compaction renderer. Runtime state and job events are
  rendered by the current state renderer and job-view serialization, which are therefore part of
  the session format: a change to either is a database version bump. Failed and interrupted
  requests keep their recorded inputs; usage identifies the originating request, including summary
  requests and failed attempts.

`SessionStore::reconstruct_model_request(sequence).await` returns the `provider/model` name the
request was issued under and the reconstructed `ModelRequest` for a `model_requested` sequence,
read from the session's journal. It checks the request's context (its agent's newest for that
purpose), checkpoint and history bound, and that an agent request follows a result for every call
its agent made, as an append does, so other records fail with a `ReplayError`. Attachments and
tool images reference content-addressed blobs by SHA-256;
`load_blobs(&mut request, &mut cache)` on the same store loads their contents for provider
encoding, reusing and then keeping exactly this request's blobs in `cache`. Both need an owning
`SessionStore`; unlike `read_records`, opening a store acquires the session's writer lock.
Reconstruction reproduces Skyhook's provider-neutral input, not an API-specific wire encoding or a
replay of the external call.

The `ModelRequest` contract preserves the typed context entries and `history_lifetime` cache hints.
See [history, transient state, and caching](architecture.md#history-transient-state-and-caching)
for their semantics and provider rationale.

Source: [configuration admission](https://github.com/kryesh/skyhook/blob/main/src/config/runtime.rs),
[session handle](https://github.com/kryesh/skyhook/blob/main/src/agent/runtime/session.rs),
[observation](https://github.com/kryesh/skyhook/blob/main/src/agent/observation.rs), and
[request reconstruction](https://github.com/kryesh/skyhook/blob/main/src/session/request.rs).

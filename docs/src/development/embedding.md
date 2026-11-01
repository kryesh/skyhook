# Embedding

The `skyhook` crate is the Rust library. Hosts supply configuration,
a workspace, model selection, and interaction handlers rather than depending on terminal UI
state. See [library architecture](architecture.md) for responsibility boundaries and the internal
request lifecycle.

## Layered host configuration and model hints

`Config::resolve(workspace, explicit).await` loads the main `config.yaml` without building
providers or looking up credentials. Hosts with another YAML schema use the same loader through
`config::resolve_layers::<T>(workspace, explicit).await`, implementing `config::LayeredConfig`
on their own deserializable type:

```rust,ignore
use std::{collections::BTreeMap, convert::Infallible};
use serde::Deserialize;
use skyhook::config::{LayeredConfig, resolve_layers};

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct HostConfig {
    profiles: BTreeMap<String, Profile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    description: String,
}

impl LayeredConfig for HostConfig {
    type Error = Infallible;
    const FILE_NAME: &'static str = "host.yaml";
    const NAMED_ENTRIES: &'static [&'static str] = &["profiles"];

    fn missing() -> Option<Self> {
        Some(Self::default())
    }
}

let loaded = resolve_layers::<HostConfig>(workspace, explicit_host_config).await?;
let host_config = loaded.config;
```

Discovery selects the first usable user file: `$XDG_CONFIG_HOME/skyhook/<FILE_NAME>`, then
`$HOME/.config/skyhook/<FILE_NAME>`. Rejected candidates remain in `loaded.report`; their fields
never enter the configuration. The workspace's `.skyhook/<FILE_NAME>` overlays that file, or can
stand alone. A malformed workspace file is fatal. An explicit path disables all discovery, even
workspace resolution, and must exist.

`NAMED_ENTRIES` lists top-level maps whose entries replace same-named entries whole while keeping
other names; every other top-level value is replaced whole, and defaults apply after merging. A
schema's `admit_layer` validates each candidate file and the effective document; `LayerSources::entry`
names the file behind a named entry for source-relative paths. The generic loader does not
interpret `providers` or `mcp`; those belong to the main `Config` schema.

Omitting `missing()` requires a discovered file. Returning defaults permits absent discovered
files, but never hides rejected files or a missing explicit path; the defaults still undergo
admission. Empty YAML, `null`, and non-mapping documents are always invalid.
`LayerResolutionError` names the schema file and retains its `ConfigReport` through `report()`.

Before sealing a `Config`, hosts can edit a configured model's profile through
`model_profile_mut(&ModelRef)`, which returns `ConfigError::Model` for an unknown provider or
model. Clearing a hint removes that model from the choices offered to agents, not from the
configured catalog. Hosts can also assign the public `default_model: Option<ModelRef>` before
calling `into_runtime()`, which validates the selection. Edits affect the new runtime generation
only, not previously sealed configurations.

## Sharing the CLI and terminal

The `skyhook-cli` crate is the shared host. A product implements `Product` with an `Args` type
implementing `clap::Args`, a `Command` implementing `clap::Subcommand` (`NoArgs` and
`NoCommand` for unused extension points), and an `Identity`: the `HostName` its sessions are
tagged with, the `tables` it keeps in them (see below), display name, about text, the
placeholder shown in an empty conversation, a `state_file` path relative to `.skyhook` for that
product's remembered model, mode and sidebar setting, a `RootMode` (`Switchable`, or `Fixed` to keep the root in the configured default mode
and refuse `--mode`), a `Theme`, and the `HostCapabilityDeclaration`s its modes grant, which
the host declares on every session the product opens, another host's included. A theme is a UI palette of semantic roles (surfaces,
emphasis, strong, accent, success/warning/error) plus a syntax palette of explicit `Rgb`
colours for highlighted code; a
product usually supplies its own UI palette and keeps `SyntaxPalette::DEFAULT` so code reads the
same in every product. Its session arguments work at the root, under `batch` and for
`dump config`; shared `auth`, `dump skills` and `stats`, and its extra commands take only their
own arguments. Session history is always `.skyhook/sessions`. The host reads `approve_all` from
`config.yaml` itself (`HostConfig`); it is not a library setting.

A thin binary retains its own global allocator and starts in this order:

```rust,ignore
fn main() {
    skyhook::remote::askpass_main();
    // SAFETY: no threads or runtime have been started.
    unsafe { skyhook_cli::main(MyProduct) }
}
```

`main` is `unsafe` because it loads the invocation directory's `.env` and configures SQLite for
the process before starting Tokio, so the process must still be single-threaded; help and version
need no `.env` or configuration.

The extension points are:

- `Product::configure(&self, &mut Config, Args, SessionContext<'_>)` returns `Configured`: the
  product's own `ConfigReport`, which the host renders beside the main configuration's, and an
  `Arc<dyn SessionFactory>`. It edits the resolved configuration before runtime admission
  (product settings, model defaults, modes, MCP entries); `dump config` serializes the result and
  a session runs with it. `command` handles the product's own subcommands.
- `SessionFactory::form(&self, SessionContext<'_>) -> Option<ScopeForm>` is the scope a new
  session's draft is written with (`cli::form`): choice and integer fields with defaults, and a
  message used when the draft is submitted empty. The terminal shows it at the top of the root's
  conversation beside the composer, sends its values with the first message and freezes it once
  the session starts; a failed start leaves it editable. The values reach `prepare` in the
  context's `Opening::New`. Batch runs pass no form.
- `SessionFactory::prepare(&self, HarnessBuilder, SessionContext<'_>)` returns
  `HookFuture<'_, PreparedSession>`: the customized builder and an `Arc<dyn SessionExtension>`.
  `SessionContext` holds the canonical workspace, `headless`, and its `Opening`: `New` with the
  form's answers, if any, or `Resume(SessionId)`. A draft calls it on first submission; the host
  tags the builder with the product's host name and tables and calls it only for the product's
  own sessions, so a foreign session opens view-only with no extension.
- `SessionExtension::opened(&self, &SessionHandle, SessionContext<'_>) -> HookFuture<'_, ()>`
  runs before the first input; associate a new session with its product resources here, and
  finish a setup an interruption left undone. A reopened session may only be viewed, so change
  nothing else. Failure shuts the session down.
- What this launch changes about a reopened session's work (as the security product applies its
  `--workers`) goes in a `skyhook::agent::ResumeHook` registered on the prepared
  builder with `HarnessBuilder::resume_hook`; the library runs it before that session's first
  work (see [Interruption, continuation, and shutdown](#interruption-continuation-and-shutdown)).

Hooks before `opened` create no sessions and write nothing to stdout; a batch run prints the
session ID first. A product that keeps its own data in each session declares it as
`Identity::tables`, an `Extension { sql, format, seed }` the host passes to
`HarnessBuilder::session_extension`: the harness creates those tables in every session it creates
and checks their format on every one of its own it reopens. `SessionStore::summary`, given the
host and its tables, refuses one of the host's own sessions that check would refuse, so the
session switcher leaves it out and `--resume` refuses it before `prepare`, as for an earlier
session format; other hosts' sessions are still listed, view-only. The harness hands host state
and the resume hook the tables as an `ExtensionDb` on the session's own writer and read pool;
tools reach them through `ToolContext::extension()`, and the extension's terminal hooks through
the session (`SessionStore::extension()`); a write whose commit failed reports `Unconfirmed`, since it may have committed. Another host's harness opens such a session without them; `ExtensionDb::inspect` reads
them read-only without opening it. Files of the product's own go in subdirectories of the
session directory. `HookFuture` is a boxed `Send` future returning `Result<T, cli::Error>`.

### Per-session terminal views and actions

`SessionExtension::views()` returns a stable slice of `ExtensionView { title, frame }`. These
tabs follow Conversation, Requests, and Jobs and share their keyboard cycling and clickable
headers. `ViewFrame::Session` draws a view inside the session's chrome; `ViewFrame::Page { agents }`
gives it the frame below the tab strip, keeping only the footer, any prompt awaiting an answer
and, with `AgentPane::Live`, the live agents: a column beside the page on a terminal at least 100
columns wide, the usual band below it on a narrower one. `AgentPane::Hidden` leaves them out.

`refresh(view, SessionHandle, Disclosed) -> HookFuture<'_, page::Page>` loads `views()[view]`
off the UI thread, as the extension's own session sees it. A `Page` is a list of typed blocks:
`Facts` (label and value pairs), a `Pipeline` of stages, `Panels` of labelled values and
meters, and `Section`s, collapsible tables of records that each open to their detail. Cells
carry a `Tone`, not a colour. The host lays the page out with its own theme, reflowing panels
and dropping section columns by priority as the width shrinks, and owns disclosure and
scrolling, so a product needs no terminal code. `Disclosed` says which sections are open, how
many of their records to supply (`Disclosed::PAGE` at a time), and which records are open,
each with how many of its detail's continued entries to supply, likewise a page at a time: supply
records and detail only for those, and mark a detail that has more entries than it shows. The
host reads the shown view about once a second and again at once when the viewer opens or closes
something. Futures must be cancellation-safe. Sanitize untrusted text before returning it.

Foreign sessions remain view-only: the host hides the composer and title editing
and withholds the commands that would write, while inspection, transcript copy and export
remain available. `SessionHandle::host()` identifies the recorded origin and `is_view_only()`
reports whether this harness may activate it; library activation checks remain authoritative.

## Constructing a harness and session

The configuration-backed entry point is:

```rust,ignore
let model = config.into_runtime()?.select_model(&name)?;
let harness = HarnessBuilder::configured(&model, workspace, host)?
    .build()
    .await?;
let session = harness.new_session().await?;
```

`into_runtime()` admits an immutable configuration generation, including any edits the host made
to the `Config`; `select_model(&name)` binds the host's explicit choice, a `ModelRef` parsed from
`provider/model`, to that generation. Admission and selection precede provider construction,
credential lookup, and workspace access. `HarnessBuilder::configured(&model, workspace, host)`
returns a builder for the application `host` serving that configuration, which can be customized before `build().await`; hosts
supplying their own providers and model profiles can instead start with
`HarnessBuilder::new(workspace, host)`. Skills are a builder input (`skills(HostSkills)`): the
harness discovers nothing on its own, so an embedder decides whether `HostSkills::discover` runs.
A provider that needs a login reports it as a typed `LoginRequired` whose remedy names the host's
own `auth login` command (`host`, or the `program` passed to `Config::login`), including in a
session's request errors; a `LoginFlow` signs in through a host-supplied `LoginPresenter` rather
than the terminal.

The configuration-backed builder carries the configured modes and selects the configured default.
`mode(name)` selects another, and `capabilities(set)` limits what modes can grant; `build()` rejects
a selected mode the modes do not declare, whichever was set first. A builder without modes gives the
root agent that capability set directly, and no mode applies.
`SessionHandle::selection(model, mode)` issues the `Selection` a submitted message or a continued
turn carries, rejecting a `provider/model` or mode the session does not have; omitted selections
retain the active model and mode. A mode change applies to the root agent from the boundary that
consumes it, not retroactively to children it already started. A session runs the harness's modes
and capability ceiling, whatever it ran under before. A reopened session reports modes the
configuration redefined or removed in `warnings()` and changes nothing while viewed, nor
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
nor a first prompt to take one from, and leaves any other alone. `run_script` titles nothing: the
terminal titles a session its script starts with the script's path, and a headless `--script`
leaves the session untitled. `rename(Some(title))` sets the user's title, and
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

## Persona and prompt templates

A host keeps its persona and mode instructions as embedded Tera templates, which the library
renders for each agent. `skyhook::templates!` embeds each named file from the invoking crate's
`src/prompts`, as a `Source::Template`, rendered by Tera, or a `Source::Verbatim`, such as a
script shown to the model, included as written and never parsed. A file's final newline is not
part of it, so an include can sit mid-line. `Templates::with(sources, &sample)` registers them
beside the library's templates, which they can include by name, for a serializable section type
`H`.

`Prompts::host(&templates, &section, persona)` and `HarnessBuilder::prompts` give a session the
host's section, fixed for the session, and its persona template, placed in every agent's system
prompt after the built-in role text and before the runtime's workspace, tool, capability and
mode instructions. Templates are named by a `TemplateName`, the name they are registered under.
A mode whose `instructions` is `Instructions::Template(name)` is told that template; configuration spells only text, and `dump config` leaves a template out. Each template
renders from the agent's context (`role`, `available_depth`, `capabilities`, `mode`,
`location`, `job_view`) with the section's fields as top-level names, when the agent's contract
is fixed: `Prompts::edition` renders a mode's instructions for an agent entering it.

The journal keeps what rendered: a mode's edition records its instructions' text, and an
agent's prompt as sent. A resumed agent keeps its recorded prompt until a mode change rebuilds
it, and a reopened root whose mode now renders otherwise warns of a changed mode. One whose mode
no longer renders for it opens all the same, warning with the error, which the work that next runs
the root in that mode returns.

Registration refuses a template that reads a top-level name its context does not define,
counting the names its includes read, so a misspelled setting fails at registration rather than
rendering as false. Names nested in a value (`capabilities.agents`) are not checked.

## Host state

`HarnessBuilder::host_state(Arc<dyn HostState>)` lets the host add its own section to an
agent's runtime state. The harness asks for the section before each request and before a
compaction summary, places it inside the state block beside the date, live jobs and todos, and
journals it with the state. It is asked with the host's tables in the session (its
`session_extension`, so only the host's own sessions have a section) and the agent, so it can
read what the host keeps in the session and answer per agent. The host renders the text itself;
`None` adds nothing to that request. Use it for facts an agent needs current, such as a
workflow's identity, policy and progress, rather than prefixing them to the user's first
message. Keep it to the agents that act on it: the state changes between requests, so every
character of it is input a provider's prompt cache cannot reuse.

`HostState::events` gives an agent news from the host's own ordered log instead: what it holds
for the agent after `since`, the `HostCursor` the agent's last host event went `through`, or from
its start. A cursor is any non-negative `i64` position (`HostCursor::try_from`), stored as it is.
The harness asks only as a request is about to be made, so a host event never starts a turn,
reopens an answered one, keeps an invocation open or ends a `wait`: it joins the job events
delivered with that request, as `{"kind":"host", …view}` with the view the host built
(`HostView::try_from` an object naming no `kind` of its own), and commits in one transaction
with its cursor. Each event is given once, across retries, compaction and reopening; an
interrupt during the read commits nothing, and a failed read fails the request
(`HarnessError::HostEvents`). The default gives none.

## Session origin and host capabilities

`HarnessBuilder::new(workspace, host)` names the application creating a session. `HostName`
(in `skyhook::identity`) is nonblank; choose a stable tag such as the binary's name, not a
session title or mode. Tags do not change the session root: hosts
share `<workspace>/.skyhook/sessions` unless they explicitly override it.

`SessionHandle::host()` and `SessionStore::host()` expose the persisted origin;
`SessionSummary::host`, `SessionStats::host`, and `SessionEvent::SessionStarted::host` retain
it for listing, accounting, and journal exports. `resume_session` may open another host's
session for inspection; `SessionHandle::is_view_only()` identifies that case, and every call
that would change the session fails with `HarnessError::HostMismatch { session, host }`. Host
tags prevent accidental cross-application activation, not access by other processes with
filesystem write permission.

Declare application-specific capabilities with `HarnessBuilder::host_capabilities`. Declaration
alone does not grant authority: include `Capability::Host(name)` in the ceiling and the modes
that need it, just like built-ins.

```rust,ignore
use skyhook::tool::policy::{Capability, HostCapability, HostCapabilityDeclaration};

let verdict: HostCapability = "security.verdict".parse()?;
let builder = builder.host_capabilities([HostCapabilityDeclaration {
    name: verdict.clone(),
    title: "Decide verdicts",
}]);
let options = ToolOptions::default().requires(Capability::Host(verdict));
```

`Capability::title` names a capability for the user: a built-in label, a host capability's
declared title, or the name of one the given declarations leave out, such as another host's. The
terminal shows a product's capabilities under the titles its `Identity` declares.

Names have exactly two components, `<namespace>.<name>`, each starting with a lowercase ASCII
letter and continuing with lowercase letters, digits, or underscores. `build()` rejects
undeclared host names in the ceiling, any mode, native tool requirements, or MCP requirements
with `HarnessError::UndeclaredHostCapability`. Host capabilities use the existing mode, ceiling,
delegation subset, and tool gates.

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

`RuntimeEvent::ResponseChanged { agent, request }` reports that a response streamed after it was
last read; it carries no deltas. Read the response as it streamed so far with
`SessionStore::response(agent, request)` and replace it in `views.responses`; a read may run ahead
of updates still queued, which fold after it. No further one is posted for that response until it
is read again, a record of its agent applies, or a new observation is taken. Live responses are
provisional, not additional committed messages. A new attempt of the same logical request resets
its live response without removing durable attempt records. On receiver lag, obtain a fresh
observation and replace **both** the snapshot and receiver, then read again the pages on display;
continuing with only one can leave a gap or apply stale live state.

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

`inspect_output_with_captures` and `jobs().inspect_output_fields` are host inspection APIs. They do not
claim jobs or consume agent notifications. `cancel_job` is the separate operation that requests
cancellation. `warnings` and `mcp_servers` expose startup diagnostics and MCP
discovery outcomes without inserting diagnostics into model context or writing directly to a
terminal. `record_status` likewise records host-facing status, not a user message.

`TodoItem` and `TodoStatus` are exported by `skyhook::agent`. Hosts projecting
checklists from records apply `SessionEvent::TodosReplaced` and the reconciled todos in
`SessionEvent::Compaction` checkpoints in sequence. Both replace the owning agent's entire list;
child-agent lists remain independent.

## Interruption, continuation, and shutdown

`prompt` and `prompt_with_options` submit input and wait for its turn. `interrupt` stops active
turns while retaining child jobs for continuation, and returns how many turns and unstarted child
agents it stopped, and calls it withdrew from waiting on the resume hook. If it stopped any turn or
agent, job deliveries start no turn until
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

`HarnessBuilder::resume_hook` registers a `ResumeHook` whose `resumed(&ExtensionDb)` runs once
per launch for each of the host's own sessions with its tables that it reopens, before the first prompt,
`enqueue_prompts`, `continue_turn_with` or `run_script`; a session only viewed never runs it, nor
does one this launch created. `SessionHandle::resume_work` runs it on its own, for work the host
prepares before sending it. A failure refuses that work with `HarnessError::Resume` (the first
queued prompt says why, the rest of its batch resolve `Interrupted`), and the next work calls it
again. An interrupt withdraws every call still waiting on it, even one whose hook succeeded or
failed as the interrupt landed: each returns `Interrupted`, starts nothing, and counts in what
`interrupt` returns. The interrupt leaves its turns continuable as any interrupt does. A hook the
interrupt cut short is dropped and runs again for the next work; one that finished does not.
`shutdown` withdraws the calls waiting on it in the same way and returns only once each call's
hook has ended, so nothing the hook does follows it.

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
`warnings` says so: calls that only write, such as a title, a status or a job
cancellation, work as usual. Only running the root fails, with `HarnessError::UnknownModel` and
before anything is journaled: a script, or a direct prompt, a continuation or an
`enqueue_prompts` call of which no prompt names a configured model. A selection naming one
replaces the model, as does `SessionHandle::activate_on`, which a viewed session journals in the
commit that activates it. Job deliveries that would wake the root before then begin a turn that
fails as any failed turn does, journaled, and stay pending for the input that names a model. A child whose own model is gone restarts
on the root's, journaled as a model change; with the root's gone too, its restart fails alone.

Session database formats are clean breaks (the current one is named in
[architecture](architecture.md)): earlier formats are not migrated and cannot be resumed; retain a
compatible Skyhook version to inspect them, or start a new session.

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

An embedder that runs the library without `skyhook_cli::main` may call
`skyhook::db::configure_process()` once at startup, before any other thread or SQLite use. It turns
off SQLite's allocation statistics, whose global mutex serialises parallel readers, but it also
disables SQLite memory accounting for everything else in the process, so the library never calls
it on its own.

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

Source: [configuration admission](https://github.com/kryesh/skyhook/blob/main/crates/skyhook/src/config/runtime.rs),
[session handle](https://github.com/kryesh/skyhook/blob/main/crates/skyhook/src/agent/runtime/session.rs),
[observation](https://github.com/kryesh/skyhook/blob/main/crates/skyhook/src/agent/observation.rs), and
[request reconstruction](https://github.com/kryesh/skyhook/blob/main/crates/skyhook/src/session/request.rs).

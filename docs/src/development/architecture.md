# Library architecture

The `skyhook` crate is the library. The `skyhook-cli` crate is a host of that library, not a
separate agent implementation. Provider protocols, terminal interaction, and
local or remote execution meet at shared runtime boundaries.

See [embedding](embedding.md) for host API and replay contracts,
[remote transport and shims](remote-transport-and-shims.md) for execution transport, and
[jobs and agents](../scripting/jobs-and-agents.md) for delegation behavior.

## Ownership boundaries

| Owner | Responsibility |
| --- | --- |
| `config::RuntimeConfig` | An admitted, immutable configuration generation. A selected model retains the configuration that admitted it. |
| `agent::Harness` | Configured models with their provider factories; policy and interaction handlers, configured tools, instructions, modes, and the capability ceiling every session it opens runs under. |
| `agent::SessionHandle` / session runtime | One agent tree, its registry and jobs, target routing, MCP connections, observation stream, and session store. |
| `AgentContext` | One agent loop's projected history, request settings (its `ModelContext`), and owned provider context. |
| `provider::ProviderContext` | Conversation-scoped provider resources and one-attempt model invocations, not authoritative conversation history or retry policy. |
| `tool::executor::ToolExecutor` | Shared admission, routing, authorization, and supervised execution for model and script tool calls. |
| `session::SessionStore` | Durable events, content-addressed blobs, and saved job outputs and captures. |

A session's isolated runtime state does not imply filesystem isolation. Execution locations
select machines and workspaces; tools still operate on those machines' files.

## Shared CLI host

`skyhook-cli` is the one terminal and batch host: command admission, `.env` loading, the shared
`auth`/`dump`/`stats` commands, batch execution, the terminal UI, UI preferences and embedded
shims. A product binary supplies a `Product`: an `Identity` (host name, branding, theme, UI-state
file, whether the root's mode is fixed), its session arguments and optional commands, a
configuration edit applied before admission, and a `SessionFactory` that prepares each of its own
sessions and may ask for a scope form before the first message. Sessions carry the creating host's
name; another host's session opens view-only, and the library refuses to activate it. All products
share `.skyhook/sessions`, the shims and the capability rules; see
[sharing the CLI and terminal](embedding.md#sharing-the-cli-and-terminal).

### Security research product

`skyhook-security` owns its layered `security.yaml` (loaded by the shared layered loader),
embedded research modes replacing presets whole (their instructions and the persona are
templates rendered for each agent with the run's live policy in its context, so only a run that
may work against a running target reads the live parts), model-role hints, native
tools, a report writer, and research tables it keeps in each of its sessions' own `session.db`
beside the journal as a session [extension](embedding.md#sharing-the-cli-and-terminal). A
session is the run: it records the run as it opens, with the live policy it keeps, and a resumed
session continues it. The run's test artifacts and current reports live in
subdirectories of the session directory. The [security guide](../guide/security.md) describes
the research behaviour; this section covers how it is held.

The root agent receives the run's facts as host state with each request; the agents below it get
none, and the facts they need travel in their briefs. The facts summarise the run's work in
counts; the synthesis with each exhausted subject's tasks and reply is read only on request.
Running and exhausted work depend on the state of agent jobs, which changes without a research
write, so every cached read that touches them (the facts, which host state and the Research
page share, and the page's open tests) keys on a journal watermark, the latest job transition or
finish, beside the store's write generation; what the run holds keys on the generation alone.
The Research tab renders those facts as a page of typed blocks, which the host lays out once per
page, disclosure and width and only turns its spinners as it draws.

The store is one graph. Every object is a `node` of one kind (component, path, state, premise,
flaw, candidate, finding) with a title and a body; a per-kind detail table shares its id, and a
composite key to `node(id, kind)` makes the schema guarantee a detail row's kind. A state's scope
is fixed in its row, and only an input state has the detail row holding its trust and defining
location, keyed to the state and its scope. Edges are tables between detail rows: a path's ordered
steps through components and its accesses (a read or write of a state at a location), the
premises a flaw or a path relies on, the premises a premise rests on (`premise_basis`, never
itself, so a merge drops a basis between the pair it merges), the states a flaw leaves wrong,
and a candidate's ordered chain of flaws, whose first is
its root, and its ordered actions, the paths its scenario performs from where the attacker
starts; a chain's flaws are not bound to an action. Ordered edges keep positions 0..n-1 through
the writes that move them. A duplicate of any kind but a finding merges. A merge repoints every
edge of the merged node to the survivor, but a merged path keeps its steps and a merged
candidate its chain and actions as history, the survivor appending after its own actions each
path the merged one performs as often as it does beyond them; a merged input state gives up its
detail row, so only live input states hold locations, and a merged state its check. `merged_into` is one hop, so readers never resolve
merges on edges, and a merge marks each node it repoints as only concerned, so the merges marking a
retired node name the node it lived on as at any event; an id argument naming a merged node resolves to the survivor at the boundary. Inventory recorded in error is retracted rather than deleted: a
path's one status is unswept, swept or retracted, and a state's or component's recorded or
retracted. A retracted node keeps its edges and shows as retracted, but every queue, coverage
count, synthesis list and derived standing reads only recorded states and components and
unretracted paths: a premise a retracted path relies on owes it no settling look and stands in no
chain of a candidate performing it. A status set back restores it.

A `location` is a supertype in the same shape: a kind and a detail row sharing its id, for a code
region, an address range of a binary image (link-base addresses stored by their bit pattern,
their span checked in Rust) or an operation and field of a named interface. Each is recorded once
per key, and input states, accesses, flaws, premises, path steps and evidence each hold one
location of any kind; a location defines one live input state, so recording one where one is
already recorded returns that one, and a merged one's location is free for the survivor or a new
one. A
component owns its interfaces by name as it owns its files, and an environment records the
interfaces it serves with their addresses; evidence naming an environment is an observation made
there.

Every write is one `event` in one audit log, by the journal's agent row, with the actor's note,
and `event_node` names each node it bears on with the strongest of four ordered effects:
`concerns`, `disputes`, `standing` or `changed`. A write names its direct marks, and one rule carries them
along the graph as it stands after the write: a premise moves the standing of the flaws and
paths relying on it, a state passes its effect to the paths accessing it, and a flaw or path
passes its effect to the live candidates chaining or performing it. Some marks stand on their
node alone: a candidate taking or losing a flaw as its root marks that flaw alone, so its other
candidates are re-owed only when the flaw itself changes, an access recorded or removed marks
its state alone, so it moves only the state's check, and a status set on a path, state or
component marks that node alone, with a state's paths, so it moves only their sweeps; but a path
retracted or restored moves its standing along the graph, since its premises leave or rejoin its
candidates' chains. A chain is edited whole through its candidate; a flaw joining or leaving
it at another place is only concerned, and a chain's work also rests on the verdicts of the
candidates holding its flaw, on the chain edits it joins or leaves, so a disproval or a leave
releases the flaw with fresh tries, and on the accesses recorded or removed of the states it
leaves wrong. Premise and flaw statuses, assessments and verdicts are histories of rows keyed by
the event recording each (`premise_decision`, `flaw_judgement`, `candidate_assessment`,
`candidate_verdict`); a finding's `reported` and a state's check are recorded in place. A result
stands on the basis of the task of its phase on its subject it was recorded within, so another
agent's change after the task began owes the work again; one recorded outside such a task stands
on nothing (`stamp`), and a report or a test's verdict outside one on its own event. Each row
records whether its owning work recorded it (`owned`), decided once at the write: a code
premise's settle, a deployment premise's answer outside every task, a flaw's chain, a
candidate's assessment and review. The standing result is the latest owned one. An owned one
that restates it, current or not, only concerns its node, so nothing resting on it moves, while
its task's basis still makes it current. A later one
recorded elsewhere that differs from it is a proposal (`proposed`), which owes its phase and marks
its node `disputes`, carried nowhere; one that agrees only concerns its node, as does one repeating
an earlier proposal while the first owned result after that one stands on the phase's current
basis, so anything moving the basis since asks it again. A retired node keeps
its own results, standing as its retirement left them: a task on a node merged away since is on
the node it lived on as when each result was recorded, the one the latest merge retargeting it
(retiring or repointing it, `node_merge`) pointed it at before then, and the merge retiring a node
is no edit of it.

What each phase's work on a node reads is its phase's arm of one relation, generated per phase with
the node bound in every arm so it seeks by node (`bearing`): standing or changed events for a sweep,
check, settle, chain or report (the root's and each current member's), with the disputes of its
result for a settle, chain, assessment or review, and, for a settle, the
decisions and disputes of the premises it rests on, those and the environments
for any candidate serving an interface the path reads an input state on (any such one when it
reads none on an interface) for a probe, but none recorded within a probe, changed events for an assessment,
all but test records and other results' disputes for a review, and assessments, test records and environments for a test, with, as its edits, a verdict of
another value and a change of its candidate's chain or actions (a standing or changed mark on the
candidate but evidence changing it, its flaws or its paths). `event_node` holds its event's kind, and its effects are indexed without test records, which
arms read from `test`, so a walk down a node's effects never visits its tests. Every arm leaves out what was recorded within a task of that phase on the node: `event_task`
names the tasks of the outermost agent holding any, the event's own or one above it, when it was
recorded, so a task begun beneath another belongs to the enclosing one, and a phase's own work,
its helpers' included, never moves what it rests on. Two events are never left out: a merge,
which work on the survivor rests on whoever made it, and a pending test's record, which a test
resumes rather than repeats. What is fixed when an event is recorded is stored with it, append
only: the work it was recorded within, with each task's basis (`event_work`), and its arms that
read only its own effects and test records (`event_bearing`, whose role marks a dispute or a
material edit). Arms through relations a later event may change (what a premise rests on, a
flaw's candidates and states, a root's members, environments, which a merge moves to its
survivor) are read when asked, and every aggregate stays derived. A basis is that relation's latest event, or the node's latest while the
relation has none, so owed work always rests on one; work is owed while its result predates its
basis, disputes aside, which give it fresh tries but leave a standing result current, or, for a verdict, once the standings it claims (`candidate_core`) no longer hold, and a
notice is one of its events after a task began. Candidate standing (`candidate_state`) and confidence are
derived views over those results, the chain's flaw and premise statuses, and the
root group's demonstrations. A premise's or flaw's standing status counts once set at or after
its latest material edit or merge into it, but those made within its own settle or chain
(`premise_standing`, `flaw_standing`); an edit since reads it as undecided or suspected again, so its settle or chain is
owed again. A flaw carrying a proposal owes its chain even while a live candidate holds it. One settling test per candidate
(`settling_test`: its latest that is not pending when that is a decline, which stands across
verdicts, or else its latest with no edit of the test's bearing after its work began) decides its demonstration, its
testing's settlement and what its report states; `finding_scenario` is the validated and noted
members a root's finding reports. Each report write records the members it covered, and each
report task the members it was offered as it began (`report_scenario`); its finding is current
only while the covered ones are those `finding_scenario` lists now: a restated verdict only
concerns its candidate, yet can make it reportable again. The begin of a report task offered
other members than its group has now joins the report's bearing as a dispute does, giving it
fresh tries however its members changed, without making a current finding stale.

Scheduling is derived; nothing is planned. Work started is a log: a `task` belongs to the
journal job of the agent that began it (an agent without a job cannot begin), and its
`task_node` rows name each subject with its basis, several for a bundle; the keys pin a
subject's kind to its phase's. A task has no end of its own: its state is the job's
`job_status`, whose index of live jobs is what live work is read from, never the task
log's history; its work is done when its subject is no longer owed. A subject is offered
while its phase owes it, no task of it is live, and fewer than two of its tasks began at its
current basis; `startable` pages the offered entries in pipeline order, leaving out what running
work holds and later work an earlier ready bundle touches, and admitting tests by the live
policy, approvals and unblocked pending tests. A bundle is the ready sweeps of the paths keyed
on one state, a path's key being the recorded handed state it accesses that the most paths
access, else such an instance state, ties to the lowest, a path with none swept alone; one path's ready settles with every premise they rest on at any
remove, or its ready chains, a shared stage's going with its lowest path and a premise with the
lowest path of itself and the premises resting on it; or one root group's ready reviews. A
check is ready once no path whose first event is no later than its basis owes a pending sweep,
whatever states the path accesses: the earliest such path is read once per statement, so a path
recorded since never holds a check back, and its sweep's accesses owe the check again. A check is
owed while its state's basis is later than the basis of the check recorded last (`state_check`),
or that check was recorded outside one.
A settle is owed while a live flaw or unretracted path relies on its premise or, through
`premise_basis` at any remove, on a premise resting on it; a decided premise owes it again once
a premise it rests on is decided or disputed after the stamp of its decision, outside its own
settle. A flaw that is not refuted and leaves a recorded state wrong owes a chain while its chain
basis is later than that of its latest chain task, whatever its candidates: one look covers
every reader, and an access recorded since owes one more. The run's scope is the components created
outside every task (`scope`: the first event naming one has no `event_task` row); a path is
outside it when its entry step's component and the owner of its file through `component_file`
are both outside it. Every phase's owed predicate skips a path outside it, a state, or a flaw by
the paths through its component and its candidates' entries, that only such paths reach, a
premise only such work relies on, and a candidate whose entry is such a path; nothing records
the scope. A path is swept when one of its reads of a recorded input state
sits in code and probed otherwise, and a probe is owed only while the launch does live work and
an environment for any candidate serves every interface it reads one on, or it reads them only
in binaries. Test selection reads each root group's facts once per
statement, and only for the groups of the subjects a read can list; an unproven candidate is
selected instead by the code premises of its chain left undecided by an exhausted settle, each
through the most severe unproven candidate resting on it whose test is not exhausted; a page reads the subjects
past its cursor in windows that double until it fills. The orchestrator's scheduler keeps only its own run's
launch replies: an entry launched twice at one basis is passed back as spent, which the one
exhaustion predicate every queue and hold shares compares with the phase's current basis as it
does a task's. The synthesis counts owed, running and exhausted work, the deployment premises
undecided while a review waits on them or carrying a proposal, the candidates standing unproven with their undecided premises, the
components no path reaches, the interfaces unswept probed paths wait on, and the paths and
candidates outside the scope. An environment recorded beneath
a test task is for that test's candidate unless it names another.

An agent with tasks is told of others' changes as a host event: for each subject of its job's tasks,
the bearing relation's events after the task began and after its cursor (for a subject merged away,
from the merge that retired it on), which leaves out those the agent or one beneath it recorded.
Each changed subject gets one digest, never omitted: the event count, the latest event and the kinds seen, with
its few newest events, their notes and their strongest marks, and each node the older events moved once, so a
delivery is bounded by subjects times recent events and the nodes their work rests on; the latest event read is
the cursor. Nothing is stored: the subscription is the task log. An
agent found with nothing to be told is skipped until research is written again. Beginning a task
returns the same digests for its revisits: for each subject a task of that phase worked before,
the bearing relation's events after the latest basis such a task began on; a first visit returns
none. A check's begin, like its state's view, also lists the decided premises of the flaws leaving
the state wrong (`flaw_state` to `flaw_premise`), with their deciding event and task and evidence
count.

`reports/` is a projection of the findings that report their scenarios as they stand, which the
orchestrator's `generate` renders from the store alone: a folder per root, replaced in one step
when what renders changed, holding the reporter's markdown beneath a generated scenario table
and a folder per stated test whose artifacts are copied only while they match the digest the
test recorded, beside a hardening list of the confirmed flaws no report covers, read in the same
snapshot as the findings: each holds no validated chain, or only chains validated below the run's
threshold.
The host capabilities `security.research`, `security.verdict` and
`security.test` gate every research write, `security.scheduled` beginning assigned work, and
`security.coordinate` scheduling and report generation; the store refuses nothing but boundary
parsing, unknown ids and integrity, so unwanted work is prevented by not scheduling it. Read-only tool actions, the host state and host
events run on the session's read pool beside its one writer, whose research callers take turns
without holding a thread; an action whose work is mostly files (recording a test, generating
reports) runs outside that turn, its transactions still taking the writer one at a time.

## Configuration and provider entries

`config` loads and layers configuration, admits it, resolves model selections, and builds the
model catalog; it does not interpret provider entries. Typed `LayeredConfig` schemas share one
loader for discovery, whole-entry merging and diagnostics; only the main `Config` schema
interprets provider and MCP provenance, stamping every provider with its defining file
(`ConfigHome`), beside which its Skyhook-managed credentials live. `provider::dialect` owns the entries
(`RawProviderConfig`, `ProviderSettings`, `ProviderModels`): each dialect module declares its
provider-only `Options` (connection rules and any Skyhook-managed `Login`) and its
inheritable request settings. Request defaults and model settings are typed partial declarations
(`provider::settings`) that keep inheritance intent until admission resolves each model into a
complete, independently validated codec and transport profile. The shared `Request` also carries
`service_tier`, a body field on every codec, `explicit_caching` for Chat and Responses, and the
Responses-only selections (`reasoning_summary`, `access_programs`); admission refuses an effective
one on any other codec, and a dialect refuses one its service does not take (Codex refuses
`explicit_caching`). Each dialect's Responses preset declares its summary default. Its
`image_limits` replace the per-image limits each dialect declares in its profile.

## Provider boundary and lifetime

`provider::Provider` is a shared factory. `open_context(id)` creates an independently owned
`ProviderContext` for one conversation without making a model request; `invoke(&mut self,
request)` returns an owned asynchronous response stream that does not borrow the context. A
startup failure is the stream's first and only item. Providers may send the context identity as
cache-affinity metadata. Custom providers implement both traits. The three inference API
families live under `provider::codec`: each codec encodes requests and decodes native streams
under a typed dialect that names the conventions varying between servers of that family (field
placements, tool-name rules, schema constraint, replay spelling, terminal-event shape), while the
codec keeps the correctness rules and reads error bodies with its API's own codes. Vendor modules
under `provider::dialect` own provider-only settings, the codecs they speak and the dialect
presets for each, credentials, and the transport conventions (fixed headers, per-turn session
headers, body fields the vendor takes whatever the codec, and typed readers that map their
servers' own error codes and wording onto error kinds); configured models run through `provider::http::HttpProvider`. Shared request,
message, and response types live under `provider::protocol`.

Each model runs as its own `HttpProvider` at its codec's endpoint under the shared API root; the
models of one provider build share its HTTP client, credential sources, command generations, and
OAuth state.

Images are admitted only when they decode whole (`media::Image`) through `image`'s own decoders,
with an allocation limit that also covers an animation's canvas, and turned upright by their EXIF
orientation. The journal keeps a still PNG, JPEG or WebP as attached; an animated PNG or WebP, and
any GIF, is kept as its decoded default image or first frame, fitted to the stored size. An `HttpProvider` context fits each image a request references to its profile's limits
once, and keeps the outcome (`media::Prepared`) while the context still references the image: a
fitted copy, or the reason none could be made. Codecs read these through a read-only view and
encode a copy with its own format, or an omitted image as a text part giving the reason, so an
image that cannot be fitted never fails the request. Only a fault in Skyhook's own fitting work,
such as a panic, fails it, as `ProviderErrorKind::Internal`.

The runtime retains the provider context across turns, tool work, questions, retries, and
compaction. A child whose invocation has resolved, with nothing left for it to take, parks: its
loop and context are released, and the input that resumes it opens a fresh context under the same
identity from its history. A model change prepares its replacement before committing the selection
and preserves journal history. Reselecting the current model keeps the current context. A mode
change also replaces the prompt and tool surface and invalidates earlier bound reasoning. A host's
persona renders into every agent's prompt; a resumed agent keeps its recorded prompt until a mode
change rebuilds it. A root reopened under a ceiling that changes the grant its prompt was rendered
for, such as a host without the user, changes into its own mode under the new grant as its next
work starts. Resume opens a fresh provider context under the same context identity, or none for a root whose
model the configuration no longer has: the session still activates for writes, and only running the root
fails with `UnknownModel` until a selection, or `activate_on`, which journals the switch in the
activation commit, moves it to a configured model. A child whose own model is gone restarts on the
root's committed model, journaled as it starts. `continue` restarts retryable children in one
commit, then claims and admits the root's held batch in one step, so nothing withdraws from it
meanwhile and a refused batch is held again unchanged while the children run on.

Prompt text is embedded Tera templates (`prompts`). An agent's prompt renders when its contract
is fixed, as it starts or changes mode, from its typed context (role, available depth,
capabilities, mode and the `skyhook_context` location), to which a host's `Prompts` add the
fields of its own section. Only a root granted `Interactive` takes the root role; a root no one
can ask is instructed as a child, working its task to a result. One render path serves both: a mode whose instructions are a host
template (`Instructions::Template`) renders them for the agent entering it into an `Edition`,
the text the journal records; the system prompt then places the host's persona, rendered for the
agent, that text and the workspace's instruction files around the library's parts, and the
journal keeps the prompt as rendered. A journal holds text, never a template name, so resume and
replay need no templates, and drift compares the mode as it renders now for the reopened root
with its recorded edition. A host's `Templates` register beside the library's; each template is
checked against its context, refusing a top-level name it does not define: a host's read the
agent's context and the section's fields, and only the system template reads the parts placed
around an agent's context. A script a template shows the model is a verbatim source, included as
written.

Each harness is built for one `identity::HostName`, the application creating sessions; the
session stores that origin and exposes it through its summary, stats and `SessionStarted`. Another
host's session can be opened for reading, and the activation boundary refuses to run or change it
with `HarnessError::HostMismatch`. Host identity does not partition the shared session root.

A host's `ResumeHook` (`agent/runtime/resume.rs`; its contract is in
[embedding](embedding.md#interruption-continuation-and-shutdown)) is ordered by the core. Each
call waiting on it joins a set in the core, in the change that also arms what leaves it, so a call
abandoned as it joins leaves too. An interrupt empties that set in the same change that stops
turns, counting what it withdrew, and so does the shutdown transition. A call is let in only by
leaving that set in a core change after the hook finished, whether it succeeded or failed, so an
interrupt that lands as the hook finishes still withdraws it.

The session owns the durable conversation (`session::Message`): user text and attachments, job
events (child replies and presented job views), parent input and compaction summaries, assistant
items, tool results, and the agent's state (date, live jobs, todos, and a host-supplied section)
as a message of its own. Providers receive it rendered:
state becomes a context state entry and job events runtime text, so the provider layer knows
nothing about jobs or todos, and those renderers are part of the session format.

Providers receive complete provider-neutral requests; they do not privately replay failed
submissions. A `response_schema` must be transmitted as a structured-output constraint or
rejected with `InvalidRequest`, not silently ignored or replaced with prompting. Input images
must be encoded or rejected explicitly; generated image outputs are not supported. Reasoning
replay carries provider/model provenance: replay from another model or API is omitted from
encoding, and so is sealed (signed or encrypted) replay from another provider configuration,
without deleting the durable transcript.

### History, transient state, and caching

A `ModelRequest` carries its context as one ordered list of typed entries: messages, and runtime
state the harness wrote, with how long the state stays in the model's view (`StateLifetime`). A
profile's `state_mode` sends each request's own state (`current`, `dynamic`), commits it to the
conversation unless it equals the latest committed one (`persisted`, `persist`), or omits it
(`none`). Everything before the first current state is a prefix later requests extend under the
same context settings until compaction replaces it. Only a state whose request produced no
response later drops out of it, since nothing is bound to it.

The journal records each request's tail. The projection gives each committed response the state
its request sent, the request of the attempt that completed with it, and later requests send
it again before that response as `superseded` state, back to the latest checkpoint or mode switch
(where bound reasoning ends). A superseded state never reaches the model, so the model always
reads only the latest dynamic state. Each codec places state in its dialect's `StateRole`: the
protocol's harness role, or user content where a server has none mid-conversation; never inside
tool output. A Messages dialect may make system-role state turn-scoped, which lets that codec
resend superseded state hidden from the model and keep the conversation unchanged for reasoning
bound to it; every other placement omits superseded state, so the wire request stops extending
the previous one where that state was, which `persist` avoids. A state not followed by a response
or the end of the context is omitted everywhere: its request produced nothing bound to it.

`history_lifetime` communicates cache reuse without prescribing a provider implementation:

- `extends` is the default: later requests extend this history.
- `detached` marks one-off settings, such as a compaction summary request, with no reusable
  history prefix under those settings.

Messages, Chat dialects that send `cache_control`, and Chat and Responses under
`explicit_caching` (`prompt_cache_options: {mode: "explicit"}`, marked with
`prompt_cache_breakpoint`) mark cache breakpoints: the cached segments of the system prompt (on
Messages and Chat), the end of the history prefix, and the point before each earlier assistant
response in it, where each prior request's prefix ended, so that prefix stays reusable. Each
dialect caps the breakpoints of one request, system segments included, at four by default, the
documented limit of each codec's API (four cache breakpoints on Messages, four cache writes per
explicit-cache request on Chat and Responses); over the cap, the oldest history breakpoints are
left out, and a request whose system segments leave no room for its history is refused. Responses
sends `cache_control` top-level instead, so the server places the breakpoint itself, and only under
automatic caching; Chat and Responses otherwise use automatic prefix caching. A detached request
marks no history, and the current state and the rest of the request carry no marker. Responses
sends every tool output as a text-block array in explicit mode, so it can carry a marker; Chat
gives that form only to a tool output it marks.

## Request, response, and recovery lifecycle

1. At a request boundary, the runtime applies accepted input and model/mode selections, every
   batch queued by then committed with its selection as one submission, refreshes history from the
   journal, and builds the request. Shared settings are recorded in
   `model_context`; `model_requested` freezes the exact history range and inline tail.
2. Each provider invocation gets a `model_attempt_started` record for that logical request.
   The stream carries display-only `Delta`s for blocks, cumulative `Usage` snapshots, and one
   terminal `End` holding the `Completion`: the complete items in position order and how the
   response ended (an answer, tool use, or a cut). Only a completion built for tool use carries
   tool calls, so nothing else can authorize execution. `LiveResponse` gives the runtime and
   observers the same provisional view of the deltas until the end arrives. Observers then see
   each response settle as committed or failed, keyed by the journal sequence it produced.
3. Only an accepted response becomes model-visible history. Its message, usage, and outcome
   commit together before tool dispatch. The response's calls are then planned concurrently and
   their jobs created together, in call order, in one commit: after a crash either every job of
   the response exists or none does, and a call that fails planning settles without a job and
   without affecting its siblings. Each tool result commits as its call finishes; request
   projection merges results back into the original call order. This preserves completed work
   after a crash without making completion order part of the next model request.
4. Once a tool exchange is closed, the runtime can compact history or build the next request.
   An agent with no further work returns to its idle loop rather than discarding its context,
   until, as a child, it parks.

The session core keeps each agent's newest request with its `RequestPhase`, the calls its
responses left without results and whether its input awaits the model; that is what turns,
activity, resume settlement and the observation read, and `session::Turns` folds each agent's
turns. The fold also decides what presenting a record reads beyond it (`session::Fact`): the
request an answer committed under, the call a result answered, and that a failed turn ended on
its request's failure. The core folds requests and each agent's state as records commit, and
writes their rows, with what it decided of answers and failed turns, in that transaction, so
request pages, statistics, the session list, agent summaries, compaction's retention and opening
a session read lifecycles from those rows rather than deriving them again.

The core admits every command by the session's `SessionPhase` (viewing, running, shutting down,
failed, closed) in one table in `session/core/phase.rs`, which also decides when what a command
journals is dated. A reopened session is only viewed until the first call that changes it
activates it (`agent/runtime/reopen.rs`), and shutdown stops a session as a crash would
(`agent/runtime/lifecycle.rs`); the host contract for both is under
[interruption, continuation, and shutdown](embedding.md#interruption-continuation-and-shutdown).

Interrupt, continue and new input read three facts the core keeps, and nothing else: the
session's flow (open or paused), each agent's turn (`Turn` in `agent/runtime/agents.rs`), and
whether each child invocation has begun. `stop_turns`, `redirect` and `interrupt_tree` in
`agent/runtime/lifecycle.rs` decide each agent from them. A busy turn an interrupt stops stays
stopping until its loop has unwound the work it took: nothing claims it, or what it would hold,
and `continue` and new input wait for that (`settle_interrupts`), so one agent never runs two
commands at once. The host's observation is a display, never an input to that decision.

Transient recovery belongs to the runtime, not the provider or transport. It classifies
normalized `ProviderErrorKind` values rather than error-message text: rate limits, timeouts,
transport failures, and unavailable servers repeat the frozen logical request. Input
and selection changes wait for the next request boundary. The failed stream is dropped before
backoff; server retry hints take precedence over capped exponential delays. Recovery remains
cancellable and continues until success or interruption. A failure is journaled with its retry
and backoff, so a request never reads failed while it retries, and one a crash leaves retrying
reopens interrupted.

A retry starts a fresh live response while retaining the attempt audit records and any reported
usage. A stream that fails or closes before its end never authorizes tool execution, and
transient recovery does not rerun completed tools. An expired command-sourced credential is
transient: the provider discards it on a 401 and the retry fetches a fresh one, and so is a
completed response with no nonblank text or call, unless it is a refusal or an output cut. Other
authentication failures, invalid requests, and protocol errors do not enter this retry loop.
Context-window recovery and unusable compaction summaries have their own bounded recovery path;
a checkpoint that breaks a journal invariant is not retried. Which cut or empty responses commit
is decided in `agent/runtime/turn.rs`.

## Compaction and context accounting

The journal is authoritative; compaction replaces the model-visible projection, not old records.
Automatic compaction is decided from a completed response's reported occupancy, at 90% of
`max_context - max_output`. Skyhook estimates no token counts: every size it uses is one a
provider reported. Summarization is a separate journaled request with a structured response
schema and no callable tools. Its checkpoint installs the replacement history and the reconciled
todos in one commit, which is refused when it would overwrite newer todo state, and replay uses
the stored continuation rather than rerunning the summary.

An agent's context (`agent/runtime/context.rs`) opens with its history, read from the journal by
index: its latest checkpoint with the retained sources, then its records after the frontier
folded by `Projection::apply`, the fold that goes on to apply the records the core feeds it, and
reads its history again after a checkpoint. The occupancy it compacts for is the core's. Compaction retains from that history, with indexed reads of the agent's
requests completed since its checkpoint and of the exchanges its live calls opened, so neither
reads the whole journal. `agent/runtime/compact/` owns the summary request and how it shrinks
after an overflow (`mod.rs`), the retained tail (`retention.rs`), and the checkpoint
(`checkpoint.rs`). The user-visible behavior is under
[conversation compaction](../guide/sessions-and-context.md#conversation-compaction), and the
durable reference contract under
[model-call reconstruction](embedding.md#reconstructing-model-calls).

## Tools, jobs, and execution

`ToolRegistryBuilder` registrations are typed; only MCP tools register a server-supplied JSON
schema. Typed registrations generate input and output schemas, and a unit registration completes
its job without a result; registry metadata also supplies compact result documentation to the
model and JavaScript runtime. Both call paths use the same executor, capability checks, authorization
coordinator, and job supervision rather than separate tool implementations. Every call is admitted
once, before authorization: a typed registration may declare an admission step that turns its
parsed input into the value its handler receives, so its checks run at the boundary rather than in
the handler. The admitted value names its path arguments, which are resolved in place, and derives
its permissions, so an inadmissible call never asks for approval. It may also choose the schema
its job's result follows, which presentation and scripts read and tool descriptions leave out,
for a tool whose result shape varies by action. Path admission (`tool::path`)
resolves a requested path where the call runs: relative to the workspace, through existing
symbolic links, as the tool will use it (an existing entry, a write target, or a removal). The
resolved path, not the spelling the caller gave, is what the call asks permission for, over the
exact file or a directory's descendants. A relative path continues the workspace's own resolution,
so every entry looked up on the way counts. A path refused outside the authorization root (one
that does not resolve, or a removal of the root, the workspace or a directory holding either) is
asked for as the path it would be, under a decision that may not ask anyone, and fails only once
allowed, so no prompt or error reveals what lies outside. The host planner and a remote worker assemble an
invocation's permissions by one rule, and a capability the caller lacks makes the tool unavailable
rather than denied.

`Capability::Host(HostCapability)` extends the built-in permission vocabulary with namespaced
`namespace.name` identifiers a host declares on its builder; ceilings, modes, native tools and
MCP requirements may only use declared names, and every authorization gate treats them like
built-ins. The journal records capability names as data, so another host's names read back
without its declarations. The session database is format 30; formats are clean breaks without
migration.

The executor coordinates host-owned jobs and their persistence. A job's lifecycle (queued,
awaiting approval, running, waiting for input, finished), its startup lease and its single
completion owner live in `job/lifecycle.rs` and `job/supervisor.rs`. A tool may declare a source
argument, a `TargetPath` on any target: the executor selects its location like a tool target,
authorizes the read with the call, and opens it for the call's context. A host tool whose input
places its job elsewhere, such as a child agent's target and workspace, has that location
selected before the job exists, so every view of the job reports it; a local workspace is resolved
and authorized there as one of the call's paths, while a remote one is only resolved where its
worker starts. A job
shows that workspace as written, `..` kept and no link followed, so no view of it reveals or
misstates what lies outside;
the job runs where it resolves. The local invocation layer
(`tool::invocation`) admits typed arguments and runs operations without requiring a session
database. The SSH shim reuses that layer for remote built-ins while the host retains job
ownership, policy decisions, and saved output, so remote workers never become second agent
runtimes. See [remote transport and shims](remote-transport-and-shims.md) for connections,
streaming and shutdown.

Tool failures are structured diagnostics: a typed cause plus the operation, subject, site, known
local effects, and, as a separate fact, whether a sent request may have executed, which only the
sender raises, as it sends. Tools annotate facts; `tool::diagnostic` owns the one renderer. Facts chosen nearest
the failure win, and each boundary (planning, dispatch, remote result ingestion) only fills what is
still unset: a failure carries a partial context in process and is resolved once, when it is
persisted, sent over the wire, or rendered. A worker's reported site is always rebound to the
destination of the connection it arrived on. Diagnostics are persisted with the job and rendered
per viewer; [saved job output](../reference/job-output.md#diagnostics) describes what a viewer
sees.

Saved output and model-visible presentation are separate. A finished result is saved once, as a
compact document with its large parts in captures (`job/output/saved.rs`), and read back through
bounded streaming readers (`job/output/json.rs`). The large parts are written before the finish,
staged: no reader lists them until the finish that references them commits, so that transaction
stays small whatever the result's size. A finish that prepares its output again, as a stale
finish retries, replaces what it staged before at the same field, and opening a session removes
those a finish never committed. JavaScript receives complete results, while the journal retains the exact previews
delivered to the model. [Saved job output](../reference/job-output.md) documents the user-facing
limits.

How a preview shows each field is saved with the result (`job/output/projection.rs`): what its
schema declares, complete fields and text keeping both ends, joined with what its producer
marks. Whether a value is shortened again follows one rule for every tool: a presentation
already is marked complete at its root; anything else is raw data, previewed by its fields'
presentations. A `jobs` listing, and a view fitted for a model or paging a field, are
presentations. A script's whole read of a result is raw data: its view carries that result's
saved presentations, while its own fields stay whole as the view's schema declares. Scripts
receive each response with the presentations its job saved, and these follow the values a
script returns, so a view returned by a script, or read again as the `jobs` call's own saved
output, shows its result as a direct read would.

Each script runs in a fresh QuickJS runtime (`tool/javascript`) with a 512 MiB engine allocation
limit set before context creation. This does not cap Rust-side bridge or job memory; allocation
failures use ordinary JavaScript error handling. Its runtime is embedded ES
modules (`tool/javascript/js`), compiled to bytecode once per process and instantiated per
script; the submitted body is compiled separately, as its own module, so failure reports
can keep only its frames. One conversion turns tool arguments and returned values into JSON at
the host boundary (`bridge.rs`), and the host fills a response's declared absent fields with
`null` there, from the schema its job stores.

Child messages and job notifications commit to the parent's history together with their
acknowledgment, as a model call's result does with the outcome it reports, so an event that never
reaches committed history stays pending for delivery
(`agent/runtime/wait/delivery.rs`, `job/delivery.rs`). A host's events join only the delivery
before a request, read under the turn's cancellation: the agent's cursor is journal state, a
pointer from `agent_state` to the message holding its latest host event, moved by that commit,
and nothing that decides whether an agent has work to take or a wait has resolved reads them. A foreground child's replies stay queued
for its call instead. Its completed result saves them: the finish drops them from the queue, and
journals only while the replies queued are those its prepared output saves, preparing the output
again otherwise (`job/lifecycle.rs`). A call let go of before it returns, or returning anything
else, leaves them to delivery (`job/entry.rs`). A question is the owner's, whoever waits on the
job: while it is pending, it and the replies queued before it are pending for the owner
together, and a foreground wait keeps waiting for the outcome. Folding the ask record also
releases the model-level foreground call blocked on the job, the job's own or that of the
script whose calls wait on it, which then returns the job unfinished, presenting and
acknowledging neither (`job/persistence.rs`, `job/delivery.rs`). A child's final answer stays its
job's result until its owner acknowledges that completion; a restart before then supersedes the
result and queues the answer as a reply (`job/entry.rs`), which a read of the superseded result,
such as a script's own call, acknowledges in place of the restarted invocation's delivery. A job's
read holds the result in the change that captures it, before its output is collected: until that
job ends, the reply is held from its owner's notifications, and a notification read before a hold
landed commits nothing. What a
waiting job asks is part of its phase, each question by the ask job waiting for its answer: an
ask's end retires its question in the same commit, which asks the rest again or lets the job run
on, and a reply is accepted into each ask's mailbox at most once, so a repeated reply answers
nothing again (`job/questions.rs`).

Process tools leave stdin unconnected, capture stdout and stderr, and detach from the controlling
terminal (`tool/builtins/process.rs`), so a program that prompts uses the session's askpass helper,
backed by the host's sensitive prompt handler. Without the `Interactive` capability, a call gets a rejecting
helper instead. OpenSSH and sudo run a Skyhook binary as that helper, so every binary calls
`remote::askpass_main()` first: it answers the prompt and exits when started in that role and
returns otherwise. The helper, its environment and prompt attribution are described under
[authentication and prompt isolation](remote-transport-and-shims.md#authentication-and-prompt-isolation).

## Session core and transaction boundaries

Each durable session is one SQLite database, `session.db`, holding the append-only, normalized
event ledger beside its blobs, saved job results and captures; `skyhook-schema/src/session.sql`
documents the tables and the few JSON columns, and sets the format the journal's identity
carries. The `skyhook-schema` proc macro runs that DDL, with any product tables layered on it,
in an in-memory database at compile time and embeds the serialized result: creating a session
writes those bytes as its file (or copies them into memory), after checking their identity
against the journal image's, and only fills the dictionaries, whose rows come from Rust enums. Closing any descriptor of a file releases every POSIX lock the
process holds on it, SQLite's included, so one registry under one lock (`db::live`) records both
each database the process has open (its file, `-wal`, `-shm` and `-journal`, by identity and by
name) and every handle another in-process opener holds (`fs::OpenFile`, by identity). An opener
refuses a live database's file and registers its handle under the lock; a database registers
before SQLite opens it, waiting at most the busy timeout for handles on its files to close;
replacing or removing a file decides and acts under the lock, and removing a tree reserves it
under the lock, so no database registers inside it, then removes it outside; walks pass over live
files. Each open session has one core thread (`session/core/mod.rs`)
that owns the only writer and the one in-memory fold of what live work reads of the journal (each
agent's newest request, the resident jobs and agents, todos and targets, the next sequence and the
last activity, never the records themselves) and, never journaled, each running agent's turn and
inbox and every registered wait.

Commands run on that thread in order. Each plans against the current state: a `Plan` names the
entries it journals, the output it saves with them, and what it changes live. The writer
journals them in one transaction, validating each record by index against the journal ahead of
it, so no append reads the whole journal. In that transaction it folds them into the state,
refusing a transition the state does not admit, and writes the current-state rows of the jobs,
requests and agents they changed; the ledger stays the only source of history and exports. No
reader sees the state ahead of the journal or a record the state does not reflect, and a rule a
concurrent change can break, such as the stale-todos check on a compaction checkpoint, is decided
against the state it commits to.

The commands already queued when the core takes one run with it as a group, and one sync of the
write-ahead log after the last makes the group durable; the core never waits for more to arrive.
Each command still plans against the state the ones before it leave and commits its own
transaction, so a refused plan leaves the rest of the group as it would be without it. What a
command leaves to happen once it commits, such as its reply, the waits it resolves and its
deferred effects, runs after the sync: until then the state and observers may reflect a commit,
but nothing the session does in response has happened. A failed sync stops the session; each
command of the group that committed, and any drain or close behind it, reports that failure. A
stopped session holds what reopening it reads, so a transaction that failed leaves nothing of its
fold. A committed record is shared, not copied, by the observation stream, the agent contexts and every
observer, and an agent's context is fed only the records of its own agent that its history folds.

One user-visible operation is one plan. A submission is admitted when its messages commit, a
queued batch commits with its model and mode selection in one transaction, and the commit that
ends a turn also closes the calls it left without results and journals the failure that ended
it, right ahead of the turn's end, which is how the request ledger tells a turn that failed on a
request from one that failed before any. The core's turn state is the single
authority for model work: an interrupt changes it in the core, and every commit that starts work
(a request, a model attempt, a response, a delivery, queued input, a compaction, or a job a model
call starts) is refused unless the agent's turn is busy. A turn's cancellation token decides
nothing; it only aborts the I/O in flight, such as a provider stream or a process.

A registered wait declares the agents and jobs it reads, and decides from the state alone before
it takes anything. A change decides again only the waits whose agents or jobs it touched,
repeating while what they take touches more, and every wait on a session-wide change (phase, flow,
activation or a resume withdrawal); each agent's activity is likewise read again only when the change touched that
agent.

### Resident set

The core holds only the resident set, so what it holds and what a change, a request or a tool
call reads of it are bounded by live work, not by the session's history. A job is resident while
it is unfinished or interrupted, has a delivery or a reply pending, is a child that input can
resume, or launched a resident job; the resident jobs are indexed by owner and by parent. A job
let go of keeps a cancellation requested of it that the journal does not record for the rest of
the session, so a job created under it is cancelled whenever it is made; a reopen forgets it.
Every
agent's state is held; its contract and the children it has started are held while it is the
root or its loop runs, and read from the records its state names as its loop starts. A job that
leaves the set is let go with its indexes and handles: a job holds its input mailbox and
cancellation token only while it can still run, and what runs an agent job again is built only as
it restarts. Whatever addresses a job by id reads it as the core last held it, from its status
row, which is final once the core lets the job go. Opening a session reads the state rows and a
few counts by index; nothing is folded from history.

### Parking

A child's loop runs only while it has something to do or something to deliver to. Once its
invocation has resolved, its turn is idle or failed with nothing queued or held, it owns no live,
interrupted or pending job and none of its children runs, it parks in the change that finds it
so. The next change that needs it (input to its job, a `continue` that restarts it, or the restart of a job it
owns) starts its loop again, and that of each parked child above it, in that same change, so an
agent never runs two loops and none parks between that check and the work it was started for. It
starts as its journal leaves it: under the contract it ran under, with the children it started,
stopped on its failure if its last turn failed, and with a fresh context opened at a request
boundary under the same identity, so `continue` and input go on as they would have had it never
parked. A child whose job was cancelled, or whose cancellation was requested, is not started
again, and its name stays its owner's. A cancellation requested of a finished child's job itself
(the `jobs` tool or the host's `cancel_job`) is journaled, so it holds after a reopen too; a
reopen forgets one that an interrupt, an abandonment or a close requested of a finished
descendant.

### Log and reads

The ledger is only appended to: the store's writer offers no update or delete of a ledger or
dictionary row. A write the journal cannot take stops the session as a crash does. Commits do not
sync the log; the core syncs it once per group of commands. SQLite's automatic checkpoint copies
the log into the database as it grows, and the last connection to close copies and removes it.
A read holds its snapshot for that read alone, so the log stays bounded by what commits during the
longest read. A writer lock excludes competing session owners, not read-only inspection.

Reads that do not belong to the core, such as job output, the blobs a request sends, an agent's
history, history pages, request reconstruction, the title, the session list, statistics and
exports, run on read-only connections, each in one snapshot, so they never wait on the writer's
transactions; a session held in memory reads through its writer's connection instead. A session
keeps a fixed few read connections, each on a thread of its own, and a read waits for one rather
than opening another. Reads are async, and one waiting for
a connection holds no thread. The core reads on a connection of its own, so its reads never wait
behind other reads. The pool and the turn it takes are `db` primitives; the journal and the
security store each supply how their readers open. The library turns SQLite's allocation
statistics off for the process before its first connection: counting takes one global mutex,
which would serialise readers that otherwise run in parallel.

### Observation

Observation is a host view of durable records, the jobs the session state folds from them, and
live state; it is not a second source of conversation history or job state. An observation copies
only what the core holds live: the phase, each agent's newest request and activity, the resident
jobs, the responses of each agent's latest turn, context gauges, and each agent's transcript
length and usage. It costs the same however long the session is, and since the core reports the
jobs it lets go of, an observer holds what the core holds. History is read in pages on read-only
snapshots, by index. A page reads at least what the observation applied, and every later record
is an update at the next ordinal of its agent, so a host placing updates by ordinal misses nothing
and repeats nothing; a lagging host observes again and reads its pages again, never the history.
Live updates the journal does not carry, such as streamed deltas and capture notices, never wait
on the core's commands, its state lock or the database, only on the observation's lock, which a
commit takes after COMMIT to fold its records into the views and publish them, and a subscriber
holds while it copies the views. A capture write notifies observers of its run once, then again
only after something reads that run's output or the run finishes; a streamed delta likewise
notifies observers of its response once, then again only after something reads that response or
a record of its agent applies, or a new observation copies the views. A burst of writes or deltas
costs a bounded number of updates, so a token stream alone never makes an observer lag.

Source: [agent runtime](https://github.com/kryesh/skyhook/tree/main/crates/skyhook/src/agent/runtime),
[providers](https://github.com/kryesh/skyhook/tree/main/crates/skyhook/src/provider),
[tools](https://github.com/kryesh/skyhook/tree/main/crates/skyhook/src/tool), and
[sessions](https://github.com/kryesh/skyhook/tree/main/crates/skyhook/src/session).

## Terminal history window

The terminal shows the viewed agent's tab from a window of its history, so drawing reads only
memory and what the terminal holds is bounded by the view, not by the history. Each tab is one
run of a list loaded a page at a time from either end: the conversation's entries by ordinal,
keeping only the kinds it can show, so a run of records that never show costs reads but nothing
held, each page with the facts presenting it reads, which later updates extend, so an entry
shows the same whatever else is loaded; requests, an owner's jobs, and a card's children. Loads
run off the event loop, at most one in flight above the view and one below, and land as work;
what is not loaded past either end of the run shows as a loading row. The conversation's
scrollbar spans the transcript's records, each entry's rows its own record and records that show
nothing none, so what loads or is let go moves it only when a load finds an end of the
transcript, which the entry nearest that end then reaches; a list's spans the rows it loaded.
What streams after the last record spans one record more, as its reply will once committed, so
the thumb moves through a long reply as it streams. Rows that show no record lie where the next
one starts, and a position there finds the first of them, so the start of the track shows what
precedes the first entry. The track maps a position to the record there, whatever the view
shows. A position beyond the run (the scrollbar, `Home`, `End`) reads there instead, at once,
in a window of its own: what the one it replaces had in flight lands unused, and until the position is laid out, scrolling
moves it rather than the rows shown. A reading position is an anchor (an entry key and rows into
it, and the tab's own entry it belongs to: a transcript entry, a request or a root job, or the
start or end where it belongs to none), never a row, so it is found again whatever a new page,
width or recovered observation lays out; the children still to read where it belongs load
whatever their distance, and it is given up only once a page read where it lies shows it is not
there. Entries beyond a fixed budget on either side of those the view needs (what it shows and
those up to the next entry each way that shows, so scrolling reaches the next row) are let go
with what presenting them read, and with the outputs of the cards they show. A job the session
lets go of, and a request a newer one replaces, keep the view they were left with while the
window shows them, whatever order that and the pages reading them arrive in. A frame lays out
only what the window holds, and the sidebar asks the session what the viewed agent holds only
once an agent's start or mode change, or the chosen agent or mode, changes it, so a frame never
waits on a commit. A session out of view keeps only its live part, its agents' summaries and the
job output selections chosen.

## Terminal transcript selection

The terminal lays out transcript entries into rows. Each row's layout separates what it paints
as decoration (wrap continuations, code padding, disclosure glyphs, spinner gutters, diff markers,
table borders and padding) from the source text it shows. A row's source is described by spans:
which logical line they belong to, which bytes of that line's text, and which bytes of the row
show them. A table cell is a logical line of its own, so the fragments of a wrapped cell share
one line. Spans also keep the source that a row shows differently or not at all, such as a cell's
trimmed separators, a replaced wide grapheme, or an empty cell.

A selection is anchored to source, not to rows: each end is an entry key, a logical line identity
within that entry, and a byte in that line's text. Line identities come from the source and do
not depend on width, but they are identities only. A Markdown line is identified by its source
position; within a tool document, also by all the text before its Markdown block. A tool document
or plain line after an entry's header is identified by all the text before it, so inserting a
line gives every line below it a new identity; the header's own text is the entry's state, so it
takes no part. Document order is the order in which the
layout first shows each line, which reads a table's cells row by row even when their fragments
interleave on screen. Endpoints, copying and highlighting all read the same spans. Copying joins
each selected line's parts in that order, one line per table cell. Each layout of an entry orders
its lines once, when a selection first needs them, so dragging across a long entry reads only
the rows of the lines it selects.

Whenever entries are laid out again (new text, a reply settling, a resize), the selection is
re-resolved against the new rows. It survives only if its endpoints still exist and the text they
select is unchanged; otherwise it is dropped rather than moved to other text.

Source: [rendering](https://github.com/kryesh/skyhook/tree/main/crates/skyhook-cli/src/tui/render).

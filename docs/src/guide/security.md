# Security research

`skyhook-security` is a companion application built from the same library and terminal host as
`skyhook`. It researches a selected part of a target: a codebase, or without source a deployed
service or API, a network daemon or a binary, recording the flaws it finds,
how each entry point reaches them, the vulnerability candidates they support, reviews, and
empirical validation attempts. It is not a sound static analyzer: a flaw, route or candidate is
a research hypothesis, not proof that a path is executable or a vulnerability exists. Reports
identify assumptions and coverage gaps.

## Configuration

The companion reuses the ordinary user and workspace `config.yaml`: providers, model settings,
credentials, execution targets, and MCP servers are not copied into another file, and
`skyhook auth` manages the same credentials. Additional settings live in `security.yaml`, with
the same user-directory discovery and workspace overlay rules; the workspace file is
`.skyhook/security.yaml`. `--security-config PATH` selects one explicit security file and
disables discovery for that file alone; `--config PATH` independently selects the ordinary
configuration. `skyhook-security dump config` shows the effective shared configuration with
security modes, model hints, and merged MCP entries applied, without opening a run or session.
The embedded modes' instructions render for each agent, so the dump leaves them out.

Model assignments refer to the **configured** `provider/model` names, not the identifiers sent
to a provider. A model can serve several roles, and several models can serve one role.
Assignments become model-selection hints rather than mandatory bindings: orchestrators choose
appropriate models when delegating, and unassigned models are not offered as explicit
delegation choices. The roles are `orchestrator`, `explorer`, `validator`, `environment`,
`poc`, and `reporter`; a role is not a mode name. A session that starts with no role hints
configured reports it as a diagnostic.

```yaml
# Replace these names with entries in your ordinary config.yaml.
models:
  main/reasoner:
    roles: [orchestrator, validator]
    hint: Prefer for complex cross-component reasoning.
  local/coder:
    roles: [explorer]
    hint: Prefer for focused code exploration.

shortlist: high
live: ask
```

The first configured orchestrator model is the companion's default model; an explicit `--model`
or the remembered model takes precedence. The root agent always runs in the `orchestrate` mode:
`--mode` and batch `--capabilities` are refused, and the terminal offers no mode switch;
children take their modes from the agent tool.

`shortlist` is the CVSS 4.0 severity a candidate needs to enter review; it defaults to `high`.
CVSS keeps an authenticated-user flaw with limited impact below that line, so set `shortlist: medium`
for web and other logic-heavy targets where such findings matter. The severity a candidate is
assessed at is the impact if its unresolved conditions hold, so uncertainty never lowers it.
`workers` is how many agents the orchestrator's scheduler runs at once (default 6). It
is the default for a new run; the run keeps what it started with, and a later launch's
`--workers` can change it. It guides the agents and is not an enforced cap.
`live` decides whether agents work against a running target, probing it and demonstrating
findings: `ask`, `yes` or `no`; omitting it asks interactively and defaults to `no` in batch
mode. See [`security.example.yaml`](https://github.com/kryesh/skyhook/blob/main/security.example.yaml)
for the complete shape.

Security settings can also add MCP servers (replacing same-named `config.yaml` entries) and
override embedded modes. A `modes` entry replaces that named embedded mode **whole**, including
its capabilities, instructions, and delegation hint. A parent must hold every capability its
child mode needs, which is why `orchestrate` holds `exec` and `network`. Every embedded mode holds
`read`, `mcp` and `agents`; the table below lists what each adds. All of them stay subject to the
usual approvals, and `targets` is granted by none. The companion declares five
capabilities, which follow the same ceiling and delegation rules as built-ins:

- `security.research` permits recording research: components, sources, paths, premises,
  flaws, candidates and their links, evidence, merges, statuses, assessments, and environments.
- `security.verdict` permits writing reports; together with `security.research`, review
  verdicts.
- `security.test` permits recording tests. It is separate so a PoC agent can also record
  research without deciding verdicts.
- `security.scheduled` permits beginning the work the orchestrator's scheduler assigns; only
  the modes it launches, and `orchestrate`, which launches them, hold it.
- `security.coordinate` permits scheduling the run and writing its reports out; only
  `orchestrate` holds it.

| Embedded mode | Security capabilities | Additional built-in capabilities |
| --- | --- | --- |
| `orchestrate` (root) | every security capability | `write`, `exec`, `network` |
| `target` | `security.research` | `network` |
| `question` | `security.research`, `security.scheduled` | `exec`, `network` |
| `sweep-part` | `security.research` | none |
| `paths`, `sources` | `security.research` | `exec`, `network` |
| `sweep`, `assess` | `security.research`, `security.scheduled` | none |
| `probe` | `security.research`, `security.scheduled` | `write`, `exec`, `network` |
| `validate` | `security.research`, `security.scheduled`, `security.verdict` | `exec`, `network` |
| `environment` | `security.research` | `write`, `exec`, `network` |
| `poc` | `security.research`, `security.scheduled`, `security.test` | `write`, `exec`, `network` |
| `report` | `security.research`, `security.scheduled`, `security.verdict` | `write`, `exec`, `network` |

`sources` holds `exec` and `network` for binary images and live enumeration, which most
`sources` agents, reading one code file, never use; `paths` holds them to launch `sources`. A `question` agent settling a premise the code alone
cannot decide may, under a live policy, run a bounded experiment or measurement in a recorded
environment, or have a child run it, and decide the premise from what it observed against its
control.

How far a child may delegate is set by its launch's depth. The embedded templates launch
coordinators (`sweep`, `probe`, `validate`, `poc`) with depth 2 and every other mode with depth
1, so any agent can delegate part of its work or a focused check when it is more than it can
cover well; agents may choose otherwise.

## Research pipeline

The root orchestrator drives the pipeline through delegated research modes; the research store
holds one graph of what the agents found and derives the work each part of it still owes;
nothing is planned. `target` maps the code, binaries and interfaces into components, mapping
what it cannot read by what its material shows, and records the deployment premises your
request settles, each actor's intended capabilities among them, and each interface in scope
under a short name; when your scope includes dependencies, it records each locked version a
published advisory affects as a flaw at its manifest entry. Under a live policy, when the map
records interfaces, the orchestrator arranges an environment serving them before `paths`, under
`ask` once you approve its scope. `paths` has a `sources` child record the entry points of each
file and of each interface an environment serves, each on the path its flow matches or one of
its own up to its handoff, then relinks the sources, path premises and candidate actions of paths
sharing a handoff onto one path and extends each through the components it reaches.

After `paths`, one scheduler runs every phase together within `workers`: each subject starts as
soon as its own prerequisites are met rather than when a whole phase ends. Ready work starts in
pipeline order — sweeps and probes, settles and chains, assessments, reviews, tests, reports —
and later work waits only on earlier-stage work, ready or running, that touches it; unrelated
later work fills the remaining slots. Settles and chains go out in bundles: one child takes every ready
entry of its phase that shares a node with the first. Work on a node that running work is about
waits for it, so one agent works a node at a time and the next starts with its findings in view.

| Phase | Subject | Owed while | Mode |
| --- | --- | --- | --- |
| Sweep | path | it is unswept and a recorded source entering it sits in code; a path is swept once, and a later correction of its steps owes nothing; marking it unswept offers its sweep again | `sweep` (long paths in `sweep-part` stretches) |
| Probe | path | it is unswept, no recorded source entering it sits in code, and the launch does live work and an environment for any candidate serves each interface its sources sit on, or they sit only in binaries; a new environment for any candidate serving one of those interfaces (any one, for binaries only), unless recorded within a probe, gives an unswept probe fresh tries | `probe` |
| Settle | code premise | it is undecided, or disputed since its decision, and a live flaw or unretracted path relies on it | `question` |
| Chain | flaw | it is suspected, or refuted but disputed, or confirmed and something it rests on changed, and no live candidate holds it; ready once no path's sweep or probe is pending | `question` |
| Assess | candidate | no assessment stands at what it rests on: its own, its chain's, its flaws' or its paths' material changes | `assess` |
| Review | candidate | it is assessed at or above the shortlist and no verdict stands; ready once no premise of its chain is undecided while its settle is pending or the user is asked it | `validate` |
| Test | candidate | it is validated, not noted, untested or reopened, and leads its root group; or its latest test is pending | `poc` |
| Report | root flaw | its group has a reportable scenario its finding does not report as it stands; ready once no member owes an assessment, review or test | `report` |

Each child first begins a task on its subjects; a subject with a running task is not offered
again, and one two tasks left owed while no other agent changed what it rests on is
**exhausted** (what a task's own agent and its helpers record never counts, save a pending test
and a merge),
releasing the work it held back, until that changes. A child that ends without beginning its
task, such as one whose provider fails, counts toward those two within one scheduler run, and
the orchestrator tells you what is exhausted with the children's replies. The scheduler stops
once nothing it could start is left, unless you ask it to stop sooner or give a budget; a test
waiting on an approval or an unblock comes back held, and the orchestrator asks you, then runs
it again with your answers. Every scheduling fact is in the store, so running the
scheduler again, after a stop, a crash or a resume, continues where the last run stopped. The
orchestrator runs one at a time.

A child working a task is told, with its next request, of what other agents changed of what
its work rests on since it began: each change once, as a digest per changed subject (how many
changes, the latest, their kinds, and the newest few with the nodes each moved most), so it can
re-read them before deciding. Its own changes and its helpers' are left out,
and the root, which works no task, is told nothing.

The research graph holds nodes of a few kinds, each with a title and a body:

- A **component** is implementation, by the files and named interfaces it owns; its body notes its
  concurrency: what runs its code, the state it shares, and what serialises access to it.
- A **source** is one entry point an actor drives, with its location and trust (untrusted,
  authenticated, local or privileged); a location has one source, so recording another there
  returns the one recorded, and merging a source frees its location for the source it merged
  into. A read of state an attacker-driven flow writes, such as a
  stored record or session, is a source too, trusted as its least-privileged writer;
  configuration read at startup is trusted as the operator, and a CI job others can trigger as
  whoever can trigger it.
- A **path** is an ordered flow of steps through components, each at a location with a note,
  from the sources that share it.
- A **premise** is a prerequisite: internal state or logic a flaw needs to reach unintended
  state, or a path needs to carry input there. A failing premise contradicts the candidates
  resting on it. It is code-scoped, about the target's own logic and decided by reading or
  observing it, or deployment-scoped, decided by you, and holds, fails, or is undecided.
- A **flaw** is a local defect at a location in a component, with an optional CWE, the
  premises it depends on, and a status: suspected, confirmed (a verified defect with no chain
  to an observable consequence found so far) or refuted.
- A **candidate** is an ordered chain of flaws to an observable vulnerability, its first flaw
  its root, through its **actions**: the paths its scenario performs, in order, the first where
  the attacker starts. A path may repeat, as in a race with itself. Candidates sharing a root
  flaw are its **group**, tested and reported together; a candidate merged into another adds,
  after the other's own actions, each path it performed as often as it did beyond the other.
- A **finding** is a root flaw's report.

A **location** is a code region (file and lines, in its function), an address range of a binary
image at its link base (in its symbol), or an operation of a recorded interface in that
interface's own terms (a route template, a method, a message type), at one of its fields when
narrower. A location names an interface, never an address, so it stays the same across
deployments. **Evidence** is a location with a stance on one node: it supports, contradicts or
gives context; one made in an environment names it.
Contradicting evidence after a premise's decision or a flaw's refutation disputes it and owes its
settle or chain again; an edit of its substance since reads it as undecided or suspected until it is
decided or judged again. Any agent may raise a candidate, a flaw or a premise at any time, left unsettled for the work that
checks it (except a deployment premise your request settles), and merge
a duplicate of any kind but a finding into the node it repeats, which takes its edges; restating
what is recorded changes nothing. A source, path or component recorded in error is retracted with
`set_status` and a note: it shows as retracted but owes no work, holds none back and leaves every
count, and the premises a retracted path relies on stand in no candidate's chain, until marked
`recorded` (a path `unswept`) again. Retracting or restoring a path owes the reviews and reports
of the candidates performing it again.

Candidates are assessed with a CVSS 4.0 base vector (CVSS-B) scored for the impact if every
premise of its chain holds; the native scorer applies the shortlist threshold. A build option,
configuration or deployment other than the default, or a race window the attacker must win,
makes Attack Requirements present and is a premise. Attack Vector, Privileges Required and the
other exploitability metrics come from what the attacker must hold or face before the first
action, plus anything a later action needs that no earlier action supplied; access or privilege
gained during the scenario is never a prerequisite. A score rests only on what the candidate's
premises establish: a condition that would raise a metric becomes its own candidate, the same
chain with that condition as a premise, so the root's report leads with whichever validates. A
review records **validated** (every
premise holds), **disproved** (a chain flaw is refuted or a code premise fails), **excluded** (a
deployment premise fails, so the deployment keeps a real finding out of reports) or **unproven**
(a premise stays undecided). A verdict counts while nothing it judged changed after it, and an
unproven one only while a premise of its chain is undecided: a premise decision or dispute owes
the review again, and a material change of the candidate, its chain, a path it performs or a
source entering one owes the assessment again too. A candidate's confidence is **conjectured**
while a premise of its chain is undecided or contradicts it, **established** otherwise,
**confirmed** while a positive verdict stands, and **demonstrated** once its settling test (its
latest test that is not pending, recorded at its latest verdict) is confirmed and its chain
uncontradicted. A validated candidate no more severe than a demonstrated one of its group is
**noted** rather than tested.

**Deployment premises** stand for what only you can answer. `target` records the ones your
request settles as holding or failing. One raised later and left undecided is never settled by
an agent: it holds back the reviews resting on it and is listed in the synthesis as asked. When
the scheduler returns, the orchestrator puts the asked premises, and those an unproven candidate
rests on, to you as one question, records each answer as the premise holding or failing, which
sends their candidates back to review, and runs the scheduler again.

The synthesis a scheduler run ends with lists the work each phase owes and has running, each
exhausted subject with its last agent's reply, the deployment premises asked of you, the
candidates left unproven with their undecided premises, the components no path reaches, and the
interfaces paths without code wait on an environment for (under `live: no`, every interface such
a path enters on). After each return the orchestrator also has a `question` agent review the loose
ends it lists, the premises of exhausted settles and those an unproven candidate rests on: each is
settled, given what would settle it, or reopened. Exhausted work is terminal: the orchestrator
closes the run once a pass changes nothing, with nothing owed or held, everything asked put to
you, and that review moving nothing. It then always writes the current reports out and lists
exhausted work as incomplete and each root flaw without a current report as having none. Every agent that records a conclusion (a premise decided, a flaw
judged, a verdict, a test result) checks the code around it for flaws or candidates it implies.
All of this is agent-discovered evidence and analytical conclusion, not a symbolic proof or a
substitute for empirical validation.

## Runs and sessions

Each security session is one **analysis run**: the session keeps the run's research beside
its journal, so a review today can be followed by environment preparation, empirical
validation, and reporting later, in the same session, without repeating discovery.

```sh
skyhook-security --prompt "Focus on request routing and access control"
skyhook-security runs            # list security sessions; -w DIR selects another workspace
skyhook-security runs SESSION    # the run as the Research tab shows it, every section open in full

# Continue a run: resume its session.
skyhook-security --resume SESSION --prompt "Extend the analysis to the admin interface"
```

The orchestrator runs the stages your instructions call for: a new session analyzes the
workspace from target selection onward, and a resumed one continues from its records, whether
the instructions extend the analysis, prepare an environment,
validate, test or rewrite reports. Fresh work is always owed; settled or exhausted work is owed
again only once something it rests on changed. A resumed run keeps its shortlist threshold,
live policy and workers; a `--workers` flag resizes it once its work resumes, and `--live`
applies only to a new session, so it is refused with `--resume`. A headless resume is refused
only when the run's own policy is `ask`, whatever the configuration says. Only the root agent knows the
live policy, and under `no` every agent's instructions leave out live work. The shortlist
threshold is applied by the store alone; no agent is told it.

In the terminal, a new session opens with a **scope panel** at the top of the
root agent's conversation: the shortlist threshold, the live policy, and the workers, prefilled from
`security.yaml`, flags, then the defaults. Edit it while writing the
instructions in the composer: `Shift+Tab` moves between the fields and the composer, `←`/`→`
change a choice, digits edit the workers. The first message sends the panel's values with it; with
no instructions the session starts with "Analyze the workspace." Once the run has started the
panel stays in view, frozen; a session that fails to start leaves it editable for the next try.

The Research tab (`]` from Jobs, or click its header) takes the whole screen below the tab
strip. It opens on a dashboard of the run: its threshold, live policy and workers, the synthesis,
a pipeline showing where each phase stands (what it owes, has running and has exhausted), and
panels for coverage (sources, paths swept and unswept, uncovered interfaces, components reached
or mapped only,
premises undecided, asked and disputed, flaws by status), candidates by status, and the tasks
running and exhausted in each phase. Below it, sections list the candidates, findings, paths,
tasks, flaws, premises, components and sources as the session records them; unswept paths come
first, and each path, premise, flaw and candidate shows what it owes and its last task, and
opens to its latest tasks. Tasks are listed newest first with their phase, subjects, state (as
their agent's job stands), and when they began and ended; a task opens to its brief, its agent's
reply and the records the agent wrote. Click a section's header, or choose it with `↑`/`↓` and
press `Enter`, to open it; a record opens the same way to its detail, a candidate to its whole
scenario: actions, chain, premises, assessment, verdict, tests and evidence, with an assessment
or verdict that no longer stands marked stale. A candidate whose demonstration is under way
shows as proving, and as test abandoned once the agent running it ended without recording an
outcome. An open section lists its first 50 records, most relevant first; its last row loads 50
more. The page scrolls as one, updates while agents work, and keeps what you opened. While
agents run, they stay in view beside the page on a wide terminal, or below it on a narrow one;
click one to open its conversation.

The orchestrator receives the run's state with each request: run, live policy, workers, the
plan's line (such as "owed: 4 sweep, 2 settle; 3 running; exhausted: 1 chain; asked: 1;
uncovered: 1"), and the
artifacts directory. The agents it starts receive no run state; their briefs name the IDs of
their work, and each agent that runs or builds anything gets a working directory of its own
under `artifacts/`, so parallel work never collides. A new
session (`Ctrl+X N`) starts as the command line would, on the model selected
where it was opened when the command line's configuration offers it, and otherwise on its default.

For automation, `batch` takes the same choices as flags over the `security.yaml` defaults:

```sh
skyhook-security batch --prompt "Review the HTTP header parser" --live no --shortlist medium
skyhook-security batch --resume SESSION --prompt "Review the upload paths" --workers 2
```

The run's files live in its session's directory under `.skyhook/sessions/`: test artifacts belong
under `artifacts/`, and the current reports are written under `reports/`. Sessions share `.skyhook/sessions` with `skyhook`, so `skyhook stats`
covers them; a session created by the other application opens view-only (see
[sessions](sessions-and-context.md)).

## Live work and reporting

Live work is any interaction with a running target: probing it, settling or reviewing what only
observation decides, and demonstrating findings. It happens only in an **environment**, a
running instance an agent prepared (a container started from a binary or image) or a deployment
you authorised, which records the interfaces it serves and their addresses. A prepared
environment is a template: each agent copies it into its own working directory and runs its own
instance there. A deployment cannot be copied, so its instructions say how agents share it, and
are the rules of engagement: addresses, identities by reference, rate, what live work may change and
must leave alone, and the window. Under `ask` the orchestrator records an environment for a
deployment only once you approve its scope; under `yes` it takes the scope from your request,
never broader. The store schedules around environments and enforces nothing: Skyhook's
approvals do, `fetch` per origin and `exec` per invocation. `exec` is not a sandbox, and batch
live work needs `approve_all`; with `live: yes` against production, that is your choice.

Under `live: no` no live work is owed: interfaces are never probed, and paths on them are
listed as uncovered, while a binary is still analysed by reading its disassembly. Under `yes` or
`ask` a path without source code is probed once an environment serves its interfaces; a probe
that needs what its environment lacks, such as an identity, leaves the path unswept for a later
environment.

Under `live: no` no demonstration is owed. Under `yes` or `ask` each validated scenario that is
not noted owes one until a test settles it, its root group's most severe first; with `ask`, an
interactive run asks after analytical review which findings to demonstrate, and each one
declined is recorded as such. Declining does **not** reject a finding: its report remains
confirmed and empirically untested, with the reason. A PoC agent makes an environment change the
scenario's premises admit itself, through an environment agent when the change is reusable; an
environment recorded within its work is prepared for its candidate, without making its test owed
again. A test waiting on you is recorded as pending and stays owed until you resolve what it
waits for, which lets it resume; one the environment cannot run is recorded as unavailable and
reported as untested with its reason. An inconclusive run is not a result: the PoC agent refines
its approach, delegating helpers as it needs, until it confirms or disproves the scenario. A
test is recorded as failed only with evidence that the scenario does not work as formulated,
which goes on the chain flaw or premise it bears on: that evidence sends the candidate back to
review, and the scenario is reported, or withdrawn, by the verdict review then records. A test
other than a declined one stands only while the verdict it tested, the latest when its work
began, is its candidate's latest: a new verdict, such as a re-validation after its actions or
chain changed, owes a test of its own, even one recorded while the test ran. An unavailable or
failed test is owed again once a later environment is recorded for its candidate or for none in
particular; a declined one stands, across new verdicts too, until you ask for it. A confirmed
test stands while its chain is uncontradicted. A race, a check apart from its use or a measured
difference such as timing is demonstrated over a fixed number of trials with a control run as
often, both success rates in the test's note, a measured difference confirmed only beyond a
threshold stated before the runs; a win that needed the window widened counts only when the
window exists in the unmodified build, with the rate without widening reported. A confirmed test
is expected to record an attack artifact and a control run on the same build, each a distinct
regular file under the run's `artifacts/` directory reached without a symbolic link; the report
shows a confirmation that lacks one. Execution uses ordinary approvals and capabilities: prefer
a disposable container or a deliberately configured target, and keep live validation within the
authorized environment.

A root flaw is reported once its group owes no assessment, review or test, by a `report` agent
who writes one markdown report: summary, analysis, impact, remediation, reproduction citing
test ids and artifacts, and a section per other scenario. The report is owed again whenever
what its scenarios rest on changes after it was written. `reports/` holds only the current
reports, rendered from the run's store; each root flaw with one has a folder,
`reports/<root>/`:

- `README.md`: front matter, the title, a generated table of the scenarios (candidate, paths,
  chain, vector, score, severity, status, confidence and test), then the reporter's report;
- `finding.json`, the machine-readable record the Markdown is rendered from;
- `test-<id>/` for each scenario's settling test: its artifacts under `attack/`,
  `control/`, `observation/`, `harness/` and `notes/` by their own names (numbered `-2`, `-3`
  when two share one), with `test.md`, `test.json` and `SHA256SUMS`.

`reports/index.md` and `index.json` list every current report with its severity (the most severe
of its scenarios) and its headline scenario's own severity, score, vector and confidence,
labelled apart since a demonstrated headline may be less severe than an untested variant.
`reports/hardening.md`, linked from the index while it has entries, lists the confirmed flaws no
report covers, with each one's title, location and CWE, in two groups: those no validated
candidate holds, with why no chain was found, which are not scored or tested; and those whose
most severe validated candidate is assessed below the run's threshold, with that candidate and
its severity. None are reported, and `index.json` lists them under `hardening`, each with its
reason. Every page
opens with a small front matter its text repeats, and links only by relative path. The
orchestrator writes `reports/` out with `research` `generate` once it closes the run: a root's folder is replaced in
one step when what renders has changed, a stale root's folder and anything else placed there is
removed, and a generation with nothing changed changes nothing. An artifact is copied only while
it matches the digest its test recorded; one that changed keeps the root out of `reports/` and
is reported.

## Native research tools

Research tools are bound to the session's run. Each call names an `action` and passes that
action's arguments beside it; the schema's description of `action` lists each action's
arguments, marking optional ones with `?`:

```js
await tool.research({action: "upsert", kind: "premise", title: "32-bit builds",
  body: "the library is built for 32-bit platforms", scope: "deployment"});
await tool.research({action: "begin", phase: "sweep", subjects: [3]});
await tool.research_query({action: "list", kind: "candidate", state: "validated", page: {after: 0, limit: 50}});
await tool.research_query({action: "node", id: 7});
await tool.research_query({action: "synthesis"});
```

A tool offers only the actions the agent's capabilities allow. Lists are cursor pages: follow
`next_after` until it is absent. CVSS input is a complete 4.0 base vector; the native scorer
computes the CVSS-B score and severity.

| Tool | Actions | Capabilities |
| --- | --- | --- |
| `research` | `begin` a task on its phase's subjects; `upsert` a node of any kind, `link` and `unlink` its edges, record `evidence`, `merge` a duplicate node, `set_status` of a premise, flaw, path, source or component, `assess` a candidate and describe an `environment` and the interfaces it `serves`; with `security.verdict` too, a `verdict`; with `security.test`, a `test`; with `security.verdict` and `write`, a root's `report`; and, for the orchestrator, `generate` the reports. | `security.research`; `security.scheduled` to `begin` |
| `research_query` | Read a `node` with its neighbourhood and latest tasks, a `task` with its child's brief, reply and records, `list` nodes of a kind (or environments and tasks), the event `history` of the run or a node, and the `synthesis` of owed, running and exhausted work, each exhausted subject with the tasks that tried it, and the uncovered interfaces; and, for the orchestrator, what is `startable` next. | `read`; `security.coordinate` for what can start next |

## Building

The companion is the `skyhook-security` crate of the workspace:

```sh
cargo run -p skyhook-security -- --help
```

The release build includes both applications; stage shims first when SSH execution is needed
(see [installation](../getting-started/installation.md)).

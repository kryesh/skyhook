# Security research

`skyhook-security` is a companion application built from the same library and terminal host as
`skyhook`. It researches a selected part of a codebase, recording the flaws it finds in the code,
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
poc: ask
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
`poc` is `ask`, `yes` or `no`; omitting it asks interactively and defaults to `no` in batch
mode. See [`security.example.yaml`](https://github.com/kryesh/skyhook/blob/main/security.example.yaml)
for the complete shape.

Security settings can also add MCP servers (replacing same-named `config.yaml` entries) and
override embedded modes. A `modes` entry replaces that named embedded mode **whole**, including
its capabilities, instructions, and delegation hint. A parent must hold every capability its
child mode needs, which is why `orchestrate` holds `exec` and `network`. Every embedded mode holds
`read`, `mcp` and `agents`; the table below lists what each adds. All of them stay subject to the
usual approvals, and `targets` is granted by none. The companion declares four
capabilities, which follow the same ceiling and delegation rules as built-ins:

- `security.research` permits recording research: scope, sources, components, flaws, exposures,
  candidates, assessments, and environments.
- `security.verdict` permits reporting; together with `security.research`, review verdicts and
  re-scoring.
- `security.test` permits opening and recording PoCs. It is separate so a PoC agent can also
  record research without deciding verdicts.
- `security.coordinate` permits scheduling the run and writing its reports out; only
  `orchestrate` holds it.

| Embedded mode | Security capabilities | Additional built-in capabilities |
| --- | --- | --- |
| `orchestrate` (root) | `security.research`, `security.verdict`, `security.test`, `security.coordinate` | `write`, `exec`, `network` |
| `target`, `question` | `security.research` | `network` |
| `paths`, `sources-file`, `component`, `component-part`, `path-review` | `security.research` | none |
| `validate` | `security.research`, `security.verdict` | `exec`, `network` |
| `environment` | `security.research` | `write`, `exec`, `network` |
| `poc` | `security.research`, `security.test` | `write`, `exec`, `network` |
| `report` | `security.verdict` | `write`, `exec`, `network` |

A question runs nothing: what only a run of the target would settle is left as an open premise
naming the test, for review or a demonstration to settle.

How far a child may delegate is set by its launch's depth. The embedded templates launch
coordinators (`target`, `paths`, `component`, `validate`) with depth 2 and every
other mode with depth 1, so any agent can delegate part of its work or a focused check when it
is more than it can cover well; agents may choose otherwise.

## Research pipeline

The root orchestrator drives the pipeline through delegated research modes; the research store
owns the records and derives the work each one still owes. Phases that sweep many units (files,
components, paths, questions) hand each unit to a child that records it (`sources-file`,
`component`, `path-review`, `question`). A component too large for one agent is split by its
sweeper into parts (call families or responsibilities) swept by `component-part` children; the
sweeper reconciles contracts across the parts and ends the sweep.

After `paths`, one scheduler runs every phase together within `workers`: each unit,
candidate or report starts as soon as its own prerequisites are met rather than when a whole
phase ends. Ready work starts in pipeline order — sweeps, path reviews, questions and
follow-ups, assessments, reviews, tests, reports — and later work waits only on earlier-stage
questions, follow-ups or other plan units, ready or running, that touch it (its candidate, flaws
or premises); unrelated later work fills the remaining slots. A path is reviewed once the sweeps
of the components it reaches are done; a candidate is assessed, reviewed and tested once no
path that is its own or reaches one of its flaws' components has an open review, and its flaws' components no
open sweep; a root flaw is reported once no candidate of its group owes an assessment, a review
or a test. Questions and follow-ups go out in
bundles: one child takes every ready question or follow-up that shares a flaw, guarantee, premise,
candidate or the function a contract check asks about with the first, ending each unit with
its own outcome. A unit about a record that running work is about waits for it, and so does a
candidate's work touching a running unit, so one agent works a record at a time and the next
starts with its findings in view; candidates sharing a flaw are still worked side by side. Each
unit, each candidate phase at one revision and each root's report of one set of scenarios is
retried once, and a brief names the finished work that held its entry back. The scheduler
keeps waiting while a demonstration waits on an approval, an unblock or a test slot, and stops
once nothing it could start is left, unless you ask it to stop sooner or give a budget.
When no child agents are active and the workflow's latest scheduling page offers no work but
holds tests, the root's next model request includes a reminder naming that script to check
outstanding approvals, unblocks, test slots and scope questions rather than only waiting for it
to finish. This is advisory: it does not grant approval or interrupt a tool already running.

Candidates whose flaws trace back through `enables` relations to one **root flaw** form that
root's group (a cycle of `enables` that nothing else feeds roots at its lowest flaw), so the variants of one defect are tested and reported together while agents
still raise each scenario as its own candidate. A group aims for one end-to-end demonstration
of its most severe scenario: its most severe untested candidate is tested first, the next one
once that test fails, a positive verdict no more severe than a demonstrated scenario in each of
its groups is **noted** rather than tested, and a more severe one still owes its own
demonstration.

| Phase | Work |
| --- | --- |
| Target | The selected subsystem, entry points, deployment assumptions, exclusions (each confirmed by the user), the principals (actors at a trust level) that drive input or that operations are intended for, and a provisional map of the code into components. |
| Paths | The entry points those principals can drive, one source each, grouped into paths: sources interpreted the same way up to the code they hand off to, with the components each reaches. Each source records its handoff (the call site handing its input on and the function it calls), which groups them; a source without one is listed for the paths agent to place. |
| Sweep | Each reached component once, a large one in parts: its flaws, the invariants argued for it (each of one kind, argued in one function, with the preconditions it rests on), a contract check for each bound, lifetime, progress or status contract carried across calls that the code does not establish, and the regions it left unexamined. A flaw that makes its code report a status, error or state for the wrong reason is followed to every consumer of that value, with a follow-up planned for it; each guarantee or premise it breaks is recorded as undermined, which counts as contradicting it. Every site returning a status that asks its caller to retry or supply more input, or turning a function's false or 0 into one, is recorded in the function returning the status and classified (input exhausted, invalid or other) before the sweep completes, and a guarantee argued across those retries cites every site of its status. A sweeper that splits a component traces each status or bound contract crossing its parts itself. |
| Path review | One agent per path decides for each flaw in what it reaches whether the path drives it and with what consequence, records where its callers violate a reached guarantee's preconditions as a flaw undermining it, and raises the candidates it sees, which may combine its flaws with flaws recorded on other paths through the components it reaches; they are assessed once the review ends. |
| Questions | Settle the questions, follow-ups and contract checks any agent planned. Each answer is a counterexample, a proof over every return site, or an open premise; an inconclusive answer names the premises it leaves open, and one about the code lists the sites enumerated and those not traced. |
| Review | Independently examine shortlisted candidates: settle their conditions, check each route, and record a verdict; other consequences of their flaws become candidates or follow-ups of their own. |
| Empirical validation | If authorized, prepare an environment and demonstrate each root group's most severe scenario, and any scenario more severe than its group's confirmed ones; a more severe impact a run shows is raised as its own candidate in the group, citing the run's artifacts, and other behaviour outside the scenario is recorded as evidence with a follow-up on the flaw it bears on. |
| Reporting | One finding per root flaw: the demonstrated scenario as its headline, the severity of the most severe validated or confirmed scenario, the other scenarios as variants with their own conditions, evidence, reproduction guidance, CVSS assessment, and PoC artifacts. Every fact is stated from the records; the reporter writes the prose around them (summary, analysis, impact, remediation, reproduction, a note per scenario, a narrative per test, and sections the outline lacks), which is stored with the finding. The write-up is addressed to the library's maintainer: the summary states who must control what and the conditions before the severity, the analysis walks the route to the harm in prose, the remediation leads with a suggested fix written to the project's constraints, and the reporter reads the write-up back as that maintainer would before recording it. The reporter never re-scores: scoring it finds inconsistent with the evidence is reported as recorded and flagged to the orchestrator, which may plan a follow-up. The orchestrator then writes the reports out. |

The research is a ledger of a few typed records:

- A **flaw** is a local defect or violated contract in one component, classified by the kind of
  operation and, where one applies, the guard that fails and how (missing, bypassable, wrong
  subject, insufficient, or stale), with its possible effects on confidentiality, integrity or
  availability. A flaw has no severity of its own.
- A **path** is the sources interpreted the same way up to the code they hand off to; its
  decisions serve every source on it, and a change of membership reopens them.
- An **exposure** is one path's decision about one flaw: unreachable, reachable with no
  security consequence, or contributing, with the conditions it needs and a short route of code
  locations from the entry, each marked inspected, inferred or assumed.
- A **candidate** is a security scenario built from one or more flaws and the paths it starts
  from; several flaws may establish one candidate together, and mutually exclusive outcomes are
  separate candidates. Any agent may raise one at any time.
- A **contract** is a function of a component and the kind of contract it keeps across calls,
  which question units check.
- A **premise** is a condition not yet established, on its owner: an unanswered deployment fact
  of the scope, shared by the candidates relying on it; a flaw, for a fact about the code every
  candidate built on the flaw relies on; or one candidate. It is resolved as holding or failing,
  or, unless it is a flaw's, asked of the user while the scope does not address it, and gates,
  for each candidate relying on it, either the scenario or only how one CVSS base metric scores.
- **Evidence** is a code location, naming the function containing it where it lies in one, and a
  role and a stance: it supports, contradicts, or gives context to the record it is about.

Candidates are assessed with a CVSS 4.0 base vector (CVSS-B) scored for the impact if every premise holds,
beside a confidence (conjectured, plausible or established); the native scorer applies the
shortlist threshold. A candidate's confidence rises past its assessment's to **confirmed** while
a positive verdict stands, and to **demonstrated** once a test of it is confirmed; candidate
details, queries, the Research tab and reports show it. A build option, configuration or deployment other than the default makes
Attack Requirements present and is a condition gating the scenario. A validated verdict needs
every member flaw to contribute along one of the candidate's paths, no member refuted, and
every condition gating the scenario resolved as holding; while a condition gating one metric
does not hold, the verdict needs that metric scored at its least severe value instead. A
metric's condition that stops holding after the verdict stops it standing until a
re-assessment scores that metric so and is reviewed; its tests stand. A rejection names its reason: the mechanism was disproved, or the scope's policy excludes the
finding. The scope states the deployment facts conditions resolve against: each build flag,
platform, host behaviour or input bound it admits or excludes. A deployment condition the scope
admits holds, citing that fact (still Attack Requirements present), and one it excludes is a
policy exclusion citing it; one no admitted fact grants is a policy exclusion resting on the
scope, and one no fact speaks to is asked of the user, whose answer the orchestrator records as
a new scope restating that fact admitted or excluded, which answers it, before the candidate is
reviewed again; a new scope reopens all of these; a duplicate is merged into the candidate that carries it, which takes over its
evidence, the premises the merge names and the owed work asking about no other premise; work
asking about a premise the merge leaves behind ends deferred with the duplicate. A reviewer rejecting one formulation
records the other reading it sees as its own candidate, or a follow-up for the question it
leaves open, so another reading of the same flaws is not lost. An **unproven** verdict is never
a dead end: each open condition gating the scenario gets one follow-up to settle it per
revision of the condition, planned when the verdict is recorded and whenever the condition is
added or reopened, and once every follow-up naming the candidate has ended, settled or not, the
candidate is reviewed again. The reviewer then decides each condition the
follow-ups left open, by a bounded local check (no network, no target systems), by the scope's
facts, or by asking the user, and the review ends in a verdict; another unproven verdict is
refused unless something new is open to wait on: a follow-up naming the candidate, a condition
asked of the user under the current scope, or a condition not yet followed up at its
revision.

Flaws, paths, candidates and premises carry revisions. A material change (contradicting
evidence, a new member or path, a path's membership change, a merge, a refutation or evidence
lifting it, a changed exposure decision) moves the revision, and an assessment or decision
recorded at an older one no longer stands; a new relation between flaws reopens the candidates
built on them. A changed candidate is assessed again, and a re-assessment with the vector and
conditions its verdict judged keeps that verdict, so review is owed again only when the
assessment changed; a demonstration is held to a higher bar (see
[live validation](#live-validation-and-reporting)). Restating a record changes nothing. A
refutation stands until evidence contradicts the refutation itself.

Coverage is the run's **review plan**: a sweep of each component a path reaches and a review of
each path, planned as they are recorded, plus questions and follow-ups any agent adds,
each naming what would settle it and the flaw, candidate, guarantees and premises it is about.
A contract has one open check, and stands by its latest: broken once a flaw undermines a
guarantee the check names, supported once it completed naming guarantees of its kind argued in
its function with nothing against them (a status contract's citing every site of its statuses),
open once it ended inconclusive. Where untrusted input reaches a component, a contract is owed
a check when something bears on it after its latest check ended (a retry site meaning invalid
input or another failure, for a status contract, or evidence against a guarantee it names), and
neither the agent whose record bore on it nor the one that planned the check completes it.
Every unit ends once, completed, blocked, inconclusive or
deferred, with its limitation, and never reopens on its own; the agent that added a unit
never completes it, while a unit the run plans for itself is open to any agent working it. A
change that leaves coverage undone plans a
new unit: a path whose sources change is planned another review, and a reached component given
files its sweeps did not cover, during a sweep or after it, is planned another sweep (a sweep
covers the files its component held when its agent started); other changes that need a look get
a follow-up. A scheduler run ends with the plan's synthesis: how many units are still open, or once every
unit ended, whether coverage agrees (the planned review is complete only when no path is
unchecked and no reached component uninspected; otherwise it names them), the units that ended
with a limitation, and the finding work still owed.

Coverage says how far that review reached, kept apart so one never stands for another:

| Record | States |
| --- | --- |
| Component | **mapped** (no path reaches it), **reached** (some path does), **inspected** (a completed sweep covered every file it holds) |
| Path | **unreached** (it reaches no recorded component), **unchecked** (a flaw of what it reaches still lacks its decision), **checked** |

An inspected component does not make the paths through it checked, and an unchecked path does
not reopen the sweep of unchanged code. Regions a sweep or a path review left unexamined are
listed as deferred until a later look settles them; a settlement may rest only on guarantees
with evidence inside the region. Coverage also counts contracts by where they stand, and the
load-bearing guarantees untrusted input reaches that no one has tried to break: those no
supported check names and no question or follow-up asks about, other than one ended deferred. All of this is agent-discovered evidence and
analytical conclusion, not a symbolic proof or a substitute for empirical validation.

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
validate, test or rewrite reports. Fresh work is always owed; settled work is owed again only
once something it rests on changed. A resumed run keeps its shortlist threshold and workers and
takes this launch's PoC policy. A reopened session only shown in the terminal keeps its settings; this launch's PoC policy and a
`--workers` flag apply once its work resumes. Only the root agent knows the PoC policy, and
under `no` its instructions leave out testing; a launch under another policy than the last one
sends the reopened root new instructions, so its next request is not served from the provider's
prompt cache. The shortlist threshold is applied by the store alone; no agent is told it.

In the terminal, a new session opens with a **scope panel** at the top of the
root agent's conversation: the shortlist threshold, the PoC policy, and the workers, prefilled from
`security.yaml`, flags, then the defaults. Edit it while writing the
instructions in the composer: `Shift+Tab` moves between the fields and the composer, `←`/`→`
change a choice, digits edit the workers. The first message sends the panel's values with it; with
no instructions the session starts with "Analyze the workspace." Once the run has started the
panel stays in view, frozen; a session that fails to start leaves it editable for the next try.

The Research tab (`]` from Jobs, or click its header) takes the whole screen below the tab
strip. It opens on a dashboard of the run: its revision, threshold, PoC policy and
workers, the plan's synthesis, a pipeline showing where each phase stands (target, paths,
sweep, path review, questions, review, PoC, report), and panels for coverage, candidates by
status, and the plan's units of each kind still open, completed, or ended with a limitation.
Below it, sections list the candidates, findings, paths, plan units, flaws, components, sources
and premises as the session records them; paths still unchecked and units that ended with a
limitation come first. Click a section's header, or choose it with `↑`/`↓` and press `Enter`, to open it; a record
opens the same way to its detail, a candidate to its whole scenario: sources, member flaws,
root flaws, routes, premises, assessment, review, PoC and report. Findings are listed by root
flaw. A candidate whose demonstration is under
way shows as proving, and as test abandoned once the agent running it ended without recording an
outcome. An open section lists its first 50
records, most relevant first; its last row loads 50 more. The page scrolls as one, updates while
agents work, and keeps what you opened. While agents run, they stay in view beside the page on a
wide terminal, or below it on a narrow one; click one to open its conversation.

The orchestrator receives the run's state with each request: run, revision and drift, PoC
policy, workers, the plan's line, the finding work still owed, and the artifacts directory.
The agents it starts receive no run state; their briefs name the IDs of their work. Coverage is
read on demand
(the Research tab, `runs SESSION`, and the `coverage` query) rather than with every request. A new
session (`Ctrl+X N`) starts as the command line would, on the model selected
where it was opened when the command line's configuration offers it, and otherwise on its default.

For automation, `batch` takes the same choices as flags over the `security.yaml` defaults:

```sh
skyhook-security batch --prompt "Review the HTTP header parser" --poc no --shortlist medium
skyhook-security batch --resume SESSION --prompt "Review the upload paths" --poc no --workers 2
```

The run records the Git revision and whether the tree was clean, dirty or unknown (Git could
not say) as it starts. A later launch on a different revision reports the drift in its Research
tab and research state. The
run's files live in its session's directory under `.skyhook/sessions/`: PoC artifacts belong
under `artifacts/`, and the current reports are written under `reports/`. Sessions share `.skyhook/sessions` with `skyhook`, so `skyhook stats`
covers them; a session created by the other application opens view-only (see
[sessions](sessions-and-context.md)).

## Live validation and reporting

Under `poc: no` no demonstration is owed. Under `yes` or `ask` each validated scenario that is
not noted owes one until an attempt settles it, its root group's most severe first; with `ask`,
an interactive run asks after analytical review which findings to demonstrate, and each one
declined is recorded as such. Declining does **not** reject
a finding: its report remains confirmed and empirically untested, with the reason.
An attempt waiting on the user or the environment is recorded as pending and stays owed; one the
environment cannot run is recorded as unavailable and reported as untested with its reason. A
selected attempt that does not demonstrate the finding, an inconclusive one included, is
recorded as failed: the finding is still reported, at confirmed, with the failed test and its
evidence, never as demonstrated. The failure stands until a later confirmation or a
contradiction of its scenario: an untested outcome or a renewed verdict does not clear it. An
unavailable test is owed again once a later environment is recorded, and a failed one after
either a later environment or a reassessment; a declined one stands until you ask for it, and
you can ask for any retry.
A finding published untested still owes its first demonstration once testing is enabled. A root
flaw is reported only once its group owes no demonstration, and an open follow-up about a
candidate holds its demonstration back. A finding is reported again whenever its scenarios
differ from the group's reportable ones, in either direction: a scenario added, disproved,
merged, re-scored, re-validated or re-tested after publication makes the finding stale and owes
the root a new report, and once none of the group is reportable the finding is withdrawn.
Finding listings, `runs` and the Research tab show each finding as current, stale, superseded
by a later one, or withdrawn. A confirmed demonstration needs an attack artifact and
a control run on the same build, each a distinct regular file under the run's `artifacts/`
directory reached without a symbolic link; the same rule applies when a report publishes them.
A demonstration, running or settled, stands until its scenario is contradicted: a member flaw
refuted, or a condition gating the scenario found failing or undermined. Other changes (flaw
edits, new relations, merges, new variants, a re-scoring) owe the analysis again but keep the
demonstration; evidence that only supports a finding changes nothing.
Execution uses ordinary approvals and capabilities: prefer a disposable container or a
deliberately configured target, and keep live validation within the authorized environment.

`reports/` holds only the current reports, as they stand now, rendered from the run's store,
which keeps every finding ever published with its reporter's prose. Each root flaw whose latest
finding is current has a folder, `reports/<root>/`:

- `README.md`, the report, and `repro.md`, the reporter's reproduction, which links each step's
  artifacts;
- `finding.json`, the machine-readable record the Markdown is rendered from;
- `poc-<id>/` for each test the report states: its artifacts under `attack/`, `control/`,
  `observation/`, `harness/` and `notes/` by their own names (numbered `-2`, `-3` when two
  share one), with `poc.md`, `poc.json` and `SHA256SUMS`.

`reports/index.md` and `index.json` list every current report with its severity (the most
severe of its scenarios) and its headline scenario's own severity, score, vector and confidence,
labelled apart since a demonstrated headline may be less severe than an untested variant. Every
page opens with a small front matter its text repeats, and links only by
relative path. The orchestrator writes `reports/` out with `finding_report` `generate` once its
scheduler returns: a root's folder is replaced in one step when what renders has changed, a
stale, superseded or withdrawn root's folder and anything else placed there is removed, and a
generation with nothing changed changes nothing. Between a change that makes a report stale and
the next generation the folder still shows it; a withdrawal after the last generation stays
until the next. A copied artifact that no longer matches what the finding recorded keeps the root
out of `reports/` and is reported. Untested findings are explicitly analytical; confirmed tests
are empirical. Populating unchanged scenarios again replaces the latest finding's prose. Research records and reports hold credential **references**, never passwords or
tokens.

## Native research tools

Research tools are bound to the session's run. Each call names an `action` and passes that
action's arguments beside it; the schema's description of `action` lists each action's
arguments, marking optional ones with `?`. Actions recording a new record are named
`add_<record>`:

```js
await tool.research({action: "add_scope", description: "HTTP request parsing",
  deployment: [{kind: "platform", stance: "excluded", statement: "32-bit builds"}]});
await tool.research_query({action: "unit", unit: 94});
await tool.research_query({action: "sources", page: {after: 0, limit: 50}});
await tool.research_query({action: "candidates", filter: {severity: "high"}, page: {after: 0, limit: 50}});
await tool.research_query({action: "synthesis"});
```

A tool offers only the actions the agent's capabilities allow. Lists are cursor pages: follow
`next_after` until it is absent. CVSS input is a complete 4.0 base
vector; the native scorer computes the CVSS-B score and severity.

| Tool | Actions | Capabilities |
| --- | --- | --- |
| `research` | Scopes, principals, sources, components, paths with their sources and reach, plan units with contract checks and their outcomes, guarantees with their kind, function and preconditions, retry sites and their classes, unexamined regions, flaws with their effects, relations, refutations and merges, premises and their resolutions, evidence, exposure decisions with routes, candidates with their members, paths and merges, and `assess`; and the guarantees or premises a flaw undermines; with `security.verdict` too: verdicts (validated, unproven, disproved or policy-excluded) and a reviewer's re-scoring; with `security.test`: opened and recorded PoCs naming the verdict they test. | `security.research`; plus `security.verdict` for verdicts, `security.test` for PoCs |
| `research_query` | Read every record above, a component's dossier, what a path's review still owes, a candidate's assessment, review and PoC history, findings, a root's latest finding with its write-up, and event history; read the plan, its synthesis and the coverage it reached; and, for the orchestrator, the work that can start next. | `read`; `security.coordinate` for what can start next |
| `environment` | `add_environment` (endpoints and credential references) or `list` them. | `security.research` to add, `write` to list |
| `finding_report` | `populate` a root flaw's finding with the reporter's prose; `generate` writes `reports/` from the store. | `security.verdict`, `write` to populate; `security.coordinate` to generate |

## Building

The companion is the `skyhook-security` crate of the workspace:

```sh
cargo run -p skyhook-security -- --help
```

The release build includes both applications; stage shims first when SSH execution is needed
(see [installation](../getting-started/installation.md)).

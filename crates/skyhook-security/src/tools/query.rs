use serde_json::Value;
use skyhook::{
    Bounded,
    tool::{RegistryError, ToolError, ToolRegistryBuilder, policy::CapabilitySet},
};

use super::{Action, Placement, holds, options, output};
use crate::capabilities::SecurityCapability;
use crate::store::{
    Assessment, Attempt, CandidateDetails, CandidateFilter, CandidateId, CandidateSummary,
    Component, ComponentId, Coverage, Cursor, Dossier, Event, Finding, FlawDetails, FlawFilter,
    FlawId, FlawSummary, Listing, Page, Path, PathId, PathWork, PlanFilter, PlanUnit, PocAttempt,
    Premise, PremiseId, Principal, PublishedFinding, Scope, Source, SourceId, Startables, Store,
    Subject, Synthesis, Tests, UnitId, ValidationAttempt, WorkSubject,
};

actions! {
    enum Query {
        scopes => Scopes {
            #[serde(default)]
            page: Page,
        } -> Listing<Scope>,
        principals => Principals {
            #[serde(default)]
            page: Page,
        } -> Listing<Principal>,
        source => Source { source: SourceId } -> Source,
        sources => Sources {
            #[serde(default)]
            page: Page,
        } -> Listing<Source>,
        paths => Paths {
            #[serde(default)]
            page: Page,
        } -> Listing<Path>,
        path => Path { path: PathId } -> Path,
        /// What a path's review still has to decide or place.
        path_work => PathWork { path: PathId } -> PathWork,
        components => Components {
            #[serde(default)]
            page: Page,
        } -> Listing<Component>,
        /// Everything recorded about a component, to reuse instead of rediscovering it.
        dossier => Dossier { component: ComponentId } -> Dossier,
        flaws => Flaws {
            #[serde(default)]
            filter: FlawFilter,
            #[serde(default)]
            page: Page,
        } -> Listing<FlawSummary>,
        flaw => Flaw { flaw: FlawId } -> FlawDetails,
        premise => Premise { premise: PremiseId } -> Premise,
        candidates => Candidates {
            #[serde(default)]
            filter: CandidateFilter,
            #[serde(default)]
            page: Page,
        } -> Listing<CandidateSummary>,
        candidate => Candidate { candidate: CandidateId } -> CandidateDetails,
        assessments => Assessments {
            candidate: CandidateId,
            #[serde(default)]
            page: Page,
        } -> Listing<Assessment>,
        validation_attempts => ValidationAttempts {
            candidate: CandidateId,
            #[serde(default)]
            page: Page,
        } -> Listing<ValidationAttempt>,
        poc_attempts => PocAttempts {
            candidate: CandidateId,
            #[serde(default)]
            page: Page,
        } -> Listing<PocAttempt>,
        findings => Findings {
            candidate: Option<CandidateId>,
            #[serde(default)]
            page: Page,
        } -> Listing<Finding>,
        /// The root flaw's latest finding: what it publishes and its write-up.
        finding => Finding { root: FlawId } -> PublishedFinding,
        /// Every event, or those about one flaw or candidate.
        history => History {
            subject: Option<Subject>,
            #[serde(default)]
            page: Page,
        } -> Listing<Event>,
        /// The next work that can start, in pipeline order: sweep, path_review, question and
        /// follow_up list open plan units of that kind, check owed contracts;
        /// assess, review and poc list candidates owing that work at their current revision,
        /// report root flaws whose group owes a report; poc_pending lists candidates whose test
        /// waits on the user or the environment. Reading reserves nothing. Result: {items:
        /// [{phase, subject: {kind: unit|contract|candidate|root, id}, title, about? (what the
        /// work is about: [{kind: flaw|candidate|guarantee|premise, id} | {kind: function, id:
        /// {component, function}}]), basis? ({kind: assess, revision} | {kind: review,
        /// assessment, verdict?} | {kind: test, validation} | {kind: report, generation}: the
        /// same subject on another basis is new work), bundle?: [entries]}], next?: {phase,
        /// after}, held (a test passed over waits for an approval, an unblock or a slot)}.
        startable => Startable {
            #[serde(default)]
            running: Vec<WorkSubject>,
            #[serde(default)]
            spent: Vec<Attempt>,
            #[serde(default)]
            tests: Tests,
            after: Option<Cursor>,
            limit: Bounded<1, 1000>,
        } -> Startables,
        /// The review plan's units, optionally of one kind or one standing outcome.
        plan => Plan {
            #[serde(default)]
            filter: PlanFilter,
            #[serde(default)]
            page: Page,
        } -> Listing<PlanUnit>,
        unit => Unit { unit: UnitId } -> PlanUnit,
        /// The plan against its outcomes, and the finding work still owed.
        synthesis => Synthesis {} -> Synthesis,
        /// Components by how far review reached them, paths by whether every flaw they reach
        /// is decided, what is deferred or still open, contracts, and the load-bearing
        /// guarantees no one has tried to break.
        coverage => Coverage {} -> Coverage,
    }
    unions: [filter]
}

impl Action for Query {
    /// What can start next is the coordinator's to schedule.
    fn permitted(action: Name, capabilities: &CapabilitySet) -> bool {
        match action {
            Name::startable => holds(capabilities, [SecurityCapability::Coordinate.capability()]),
            _ => true,
        }
    }

    fn placement(&self) -> Placement {
        Placement::Reader
    }
}

impl Query {
    fn execute(self, store: &Store) -> Result<Value, ToolError> {
        match self {
            Self::Scopes(a) => output::<result::Scopes>(store.list_scopes(a.page)),
            Self::Principals(a) => output::<result::Principals>(store.list_principals(a.page)),
            Self::Source(a) => output::<result::Source>(store.source(a.source)),
            Self::Sources(a) => output::<result::Sources>(store.list_sources(a.page)),
            Self::Paths(a) => output::<result::Paths>(store.paths(a.page)),
            Self::Path(a) => output::<result::Path>(store.path(a.path)),
            Self::PathWork(a) => output::<result::PathWork>(store.path_work(a.path)),
            Self::Components(a) => output::<result::Components>(store.components(a.page)),
            Self::Dossier(a) => output::<result::Dossier>(store.dossier(a.component)),
            Self::Flaws(a) => output::<result::Flaws>(store.flaws(&a.filter, a.page)),
            Self::Flaw(a) => output::<result::Flaw>(store.flaw(a.flaw)),
            Self::Premise(a) => output::<result::Premise>(store.premise(a.premise)),
            Self::Candidates(a) => {
                output::<result::Candidates>(store.candidates(&a.filter, a.page))
            }
            Self::Candidate(a) => output::<result::Candidate>(store.candidate(a.candidate)),
            Self::Assessments(a) => {
                output::<result::Assessments>(store.assessments(a.candidate, a.page))
            }
            Self::ValidationAttempts(a) => {
                output::<result::ValidationAttempts>(store.validation_attempts(a.candidate, a.page))
            }
            Self::PocAttempts(a) => {
                output::<result::PocAttempts>(store.poc_attempts(a.candidate, a.page))
            }
            Self::Findings(a) => output::<result::Findings>(store.findings(a.candidate, a.page)),
            Self::Finding(a) => output::<result::Finding>(store.finding(a.root)),
            Self::History(a) => output::<result::History>(store.events(a.subject, a.page)),
            Self::Startable(a) => output::<result::Startable>(
                store.startable(&a.running, &a.spent, &a.tests, a.after, a.limit),
            ),
            Self::Plan(a) => output::<result::Plan>(store.plan(&a.filter, a.page)),
            Self::Unit(a) => output::<result::Unit>(store.unit(a.unit)),
            Self::Synthesis(_) => output::<result::Synthesis>(store.synthesis()),
            Self::Coverage(_) => output::<result::Coverage>(store.coverage()),
        }
    }
}

pub(super) fn register(builder: &mut ToolRegistryBuilder) -> Result<(), RegistryError> {
    super::register(
        builder,
        "research_query",
        "Read the run's research, its review plan and the work it owes. Every list is a keyset page {items: T[], next_after?: integer}: continue with page.after = next_after until it is absent, even when a page is empty. A plan unit is open until an outcome is recorded and never reopens on its own; a change that leaves coverage undone (a path's membership, files added to a swept component or during its sweep) plans a new unit. synthesis compares the plan with its outcomes and, once every unit ended, with coverage. Result: JobView<T> with T per action: scopes → page of Scope {id, description, deployment: [{id, kind, stance: admitted|excluded, statement}], asked: [{premise, candidate, statement, question}] (conditions asked of the user under it that nothing decided since)} (the latest scope is the run's policy); principals/sources/paths/components/flaws/candidates/assessments/validation_attempts/poc_attempts/findings/history/plan → page of that record; source → Source; path → Path {id, description, revision, sources, reaches, coverage: unreached|unchecked|checked, deferred}; path_work → {decisions: flaw ids, unplaced: exposure ids}; dossier → Dossier {component, files, reached_by, sweeps, guarantees: Guarantee {id, component, function?, kind, statement, preconditions, retry_status?, evidence, undermined_by}, checks, retry_sites: [{id, location, status, producer?, class (null until classified)}], deferred (settled? {note, guarantees}), flaws}; flaw → FlawDetails {flaw (with misreports?), statement, deficiency, effects, evidence, relations, refutation, exposures, undermines}; premise → Premise {id, on, kind, statement, revision, resolution?, evidence, undermined_by}; candidate → CandidateDetails {candidate, description, cwe, members, roots, paths, sources, premises (each with its gates), evidence, exposures, assessment}; unit → PlanUnit {id, kind, component|path|question and stopping and candidate? and flaw? and invariants? and check?, since?, outcome (null while open; open?: [{premise, untraced?}]), planned_by, at}; finding → the root flaw's latest finding, whatever its status, {finding: Finding {id, root, headline, candidates, severity, confidence, status: current|stale|superseded (with by)|withdrawn, …}, scenarios: [{candidate, assessment, validation, poc?}] (the scenarios it published as its report states them, the headline first; a test's artifacts by recorded reference), write_up (its prose in the shape finding_report populate takes)}, not found for a root never reported; synthesis → {units: {kind: {open, ended: {outcome: count}}}, standing: {state: not_planned|in_progress|ended, unchecked?: path ids, uninspected?: component ids}, findings: {phase: count}}; coverage → {components: {mapped|reached|inspected: count}, paths: {unreached|unchecked|checked: count}, deferred, open_premises (candidates' unresolved conditions), contracts, unchallenged (load-bearing guarantees nothing tried to break)}.",
        options::<Query>(),
        |store, _, _, query| query.execute(store),
    )
}

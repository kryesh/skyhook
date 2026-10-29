use serde_json::Value;
use skyhook::{
    identity::AgentId,
    tool::{RegistryError, ToolError, ToolRegistryBuilder, policy::CapabilitySet},
};

use super::{Action, Placement, holds, options, output};
use crate::{
    capabilities::SecurityCapability,
    cvss::Cvss40,
    store::{
        AssessmentResult, Assessor, CandidateId, CandidateMetadata, ComponentId, Confidence,
        DeferredId, DeferredSubject, Effect, EvidenceId, EvidenceSubject, ExposureId, FlawId, Gate,
        GuaranteeId, Invariant, InvariantKind, Location, Member, NewCandidate, NewCheck,
        NewComponent, NewDeployment, NewEvidence, NewExposure, NewFlaw, NewGuarantee, NewPath,
        NewPremise, NewPrincipal, NewRetrySite, NewSource, OpenPremise, Outcome, OutcomeId, PathId,
        PocArtifact, PocAttempt, PocResult, PocStartId, PremiseId, PremiseOn, PrincipalId, Reach,
        RefutationId, RelationId, RelationKind, Resolution, ResolutionId, RetrySiteId, ScopeId,
        SourceId, Store, Text, UnderminingId, UnitId, UnitWork, ValidationAttempt, ValidationId,
        Verdict,
    },
};

actions! {
    enum Research {
        /// A new scope becomes the run's policy, with the facts about the deployment in scope
        /// that conditions resolve against: each build flag, platform, host behaviour or input
        /// bound it admits or excludes. Policy exclusions under an earlier scope return to
        /// review, and conditions holding by its facts reopen.
        add_scope => AddScope {
            description: Text,
            #[serde(default)]
            deployment: Vec<NewDeployment>,
        } -> ScopeId,
        /// An actor that can drive input or for whom an operation is intended. Names are
        /// unique; re-adding one replaces its trust and description.
        add_principal => AddPrincipal(NewPrincipal) -> PrincipalId,
        add_source => AddSource(NewSource) -> SourceId,
        /// Records a component, or adds files to the component with this name. Naming a file
        /// another component owns moves it here until a completed sweep covers it.
        add_component => AddComponent(NewComponent) -> ComponentId,
        /// Sources interpreted identically up to their handoff into implementation, with the
        /// components they reach; its description is that shared interpretation: entry,
        /// parsing and state up to the handoff. The path's review and a sweep of each newly
        /// reached component are planned with it.
        add_path => AddPath(NewPath) -> PathId,
        /// Sources join or leave a path: a membership change, so every decision along the
        /// path is owed again.
        set_path_sources => SetPathSources {
            path: PathId,
            #[serde(default)]
            add: Vec<SourceId>,
            #[serde(default)]
            remove: Vec<SourceId>,
        } -> (),
        /// More components a path reaches; each newly reached component is planned a sweep.
        add_reaches => AddReaches { path: PathId, reaches: Vec<Reach> } -> (),
        /// Add a unit to the review plan: a sweep of a component, a review of a path, a
        /// question or a follow-up.
        add_unit => AddUnit(UnitWork) -> UnitId,
        /// Plan a question whether `function` keeps its contract of `kind` across calls (or an
        /// owed `contract`), or join the open one.
        add_check => AddCheck(NewCheck) -> UnitId,
        /// How a plan unit ended, with what was done and its limitation; a unit has one
        /// outcome, and the agent that planned it may not complete it. A completed sweep covers
        /// the files its component held when you started.
        outcome => UnitOutcome {
            unit: UnitId,
            outcome: Outcome,
            note: Text,
            /// For an inconclusive question or follow-up, required: the premises it leaves open.
            #[serde(default)]
            open: Vec<OpenPremise>,
        } -> OutcomeId,
        /// An invariant the component is argued to enforce, the preconditions the argument
        /// rests on, and its evidence.
        add_guarantee => AddGuarantee { component: ComponentId, guarantee: NewGuarantee } -> GuaranteeId,
        /// A region of a component, or a stretch of a path, left unexamined and why.
        add_deferred => AddDeferred {
            subject: DeferredSubject,
            region: Option<Location>,
            reason: Text,
        } -> DeferredId,
        /// A later examination covered the region; the guarantees it rests on each need
        /// evidence inside it.
        settle_deferred => SettleDeferred {
            deferred: DeferredId,
            #[serde(default)]
            guarantees: Vec<GuaranteeId>,
            note: Text,
        } -> (),
        /// A site in a component's file returning a status that asks its caller to retry or
        /// supply more input, or mapping `producer`'s false or 0 to one, with what the status
        /// can mean there. Restating its region updates it.
        retry_site => RetrySite(NewRetrySite) -> RetrySiteId,
        /// A local defect or violated contract with its possible effects; no severity. Its
        /// statement names the invariant involved and how it may be violated.
        add_flaw => AddFlaw(NewFlaw) -> FlawId,
        /// Adds or restates possible effects, a material change to the flaw.
        add_effects => AddEffects { flaw: FlawId, effects: Vec<Effect> } -> (),
        /// The flaw is not a defect, for example because another component enforces the
        /// contract. Only evidence contradicting the refutation lifts it.
        refute => Refute { flaw: FlawId, reason: Text } -> RefutationId,
        /// A new relation is a material change to the candidates built on either flaw.
        add_relation => AddRelation {
            a: FlawId,
            b: FlawId,
            kind: RelationKind,
            note: Text,
        } -> RelationId,
        /// The flaw breaks this guarantee or premise: it counts as contradicting it.
        undermine => Undermine {
            flaw: FlawId,
            invariant: Invariant,
            note: Text,
        } -> UnderminingId,
        merge_flaws => MergeFlaws { duplicate: FlawId, into: FlawId, note: Text } -> (),
        /// A condition not yet established, on its owner: an unanswered deployment fact, a flaw
        /// or a candidate, gating its scenario or one metric.
        add_premise => AddPremise {
            on: PremiseOn,
            kind: InvariantKind,
            statement: Text,
        } -> PremiseId,
        /// The candidate relies on a deployment or flaw condition, gating `gates`.
        rely => Rely { candidate: CandidateId, premise: PremiseId, gates: Gate } -> (),
        /// Settle a condition: the latest resolution at its revision stands; an `asked` one
        /// holds the review of the candidates relying on it until a later scope answers it.
        resolve => Resolve {
            premise: PremiseId,
            resolution: Resolution,
            note: Text,
        } -> ResolutionId,
        /// Contradicting evidence reopens every decision about its subject; on a refutation it
        /// lifts the refutation.
        add_evidence => AddEvidence { subject: EvidenceSubject, evidence: Vec<NewEvidence> } -> Vec<EvidenceId>,
        /// One path's decision about one flaw at the flaw's and the path's current revisions.
        add_exposure => AddExposure(NewExposure) -> ExposureId,
        /// Raise a candidate at any time: its flaws (new ones inline), and the paths, premises
        /// and evidence known so far.
        add_candidate => AddCandidate(NewCandidate) -> CandidateId,
        add_members => AddMembers { candidate: CandidateId, members: Vec<Member> } -> (),
        add_paths => AddPaths { candidate: CandidateId, paths: Vec<PathId> } -> (),
        /// Descriptive edits keep every decision about the candidate.
        update_candidate => UpdateCandidate {
            candidate: CandidateId,
            metadata: CandidateMetadata,
            note: Text,
        } -> (),
        /// One root cause and one consequence are one candidate: the duplicate's flaws,
        /// paths and evidence move to `into`, which is owed reassessment, with the duplicate's
        /// premises named in `keep`; the others stay behind with the duplicate.
        merge_candidates => MergeCandidates {
            duplicate: CandidateId,
            into: CandidateId,
            keep: Vec<PremiseId>,
            note: Text,
        } -> (),
        /// A complete CVSS 4.0 base vector scoring the impact if the premises hold; the
        /// native scorer computes score and severity. Confidence carries the uncertainty.
        /// Without verdict authority a different assessment is refused once the current one
        /// has a verdict.
        assess => Assess {
            candidate: CandidateId,
            /// `CVSS:4.0/AV:_/AC:_/AT:_/PR:_/UI:_/VC:_/VI:_/VA:_/SC:_/SI:_/SA:_`, every base
            /// metric once, in that order.
            vector: Cvss40,
            confidence: Confidence,
            rationale: Text,
        } -> AssessmentResult,
        /// A verdict on the candidate's current assessment.
        verdict => Review { candidate: CandidateId, verdict: Verdict, rationale: Text } -> ValidationAttempt,
        /// Re-score the candidate at its current revision when the vector itself is wrong. The
        /// verdict on the earlier assessment no longer stands, so the candidate needs review
        /// again, while its tests at that revision stay.
        reassess => Reassess {
            candidate: CandidateId,
            /// `CVSS:4.0/AV:_/AC:_/AT:_/PR:_/UI:_/VC:_/VI:_/VA:_/SC:_/SI:_/SA:_`, every base
            /// metric once, in that order.
            vector: Cvss40,
            confidence: Confidence,
            rationale: Text,
        } -> AssessmentResult,
        /// Open a test of the candidate against `validation`, its positive verdict, before any
        /// work; the candidate shows as proving while you work.
        start_poc => StartPoc { candidate: CandidateId, validation: ValidationId } -> PocStartId,
        /// A test of the candidate against `validation`, a positive verdict at its current
        /// revision; it settles the test you opened. `failed` is any run that did not
        /// demonstrate the scenario, inconclusive ones included. `pending` waits on the user or the
        /// environment (say which in the note) and settles nothing; `unavailable` (the
        /// environment cannot run a real demonstration) and `declined` (the user turned this
        /// one down) settle it as untested, with the reason in the note.
        record_poc => RecordPoc {
            candidate: CandidateId,
            validation: ValidationId,
            result: PocResult,
            note: Text,
            /// Each a distinct path relative to the run's artifacts directory naming a regular
            /// file there, reached without a symbolic link, tagged with its role. A confirmed outcome needs an `attack` and a `control`
            /// artifact.
            #[serde(default)]
            artifacts: Vec<PocArtifact>,
        } -> PocAttempt,
    }
    unions: [kind, outcome, subject]
}

impl Action for Research {
    fn permitted(action: Name, capabilities: &CapabilitySet) -> bool {
        let [research, verdict, test] = [
            SecurityCapability::Research,
            SecurityCapability::Verdict,
            SecurityCapability::Test,
        ]
        .map(SecurityCapability::capability);
        match action {
            // A verdict judges research, so it needs both authorities; a test needs its own,
            // so an agent that tests and records what it observes never decides verdicts.
            Name::verdict | Name::reassess => holds(capabilities, [research, verdict]),
            Name::start_poc | Name::record_poc => holds(capabilities, [test]),
            _ => holds(capabilities, [research]),
        }
    }

    /// Recording a test admits its artifact files before its transaction.
    fn placement(&self) -> Placement {
        match self {
            Self::StartPoc(_) | Self::RecordPoc(_) => Placement::Outside,
            _ => Placement::Writer,
        }
    }
}

impl Research {
    fn execute(self, store: &Store, actor: &AgentId) -> Result<Value, ToolError> {
        match self {
            Self::AddScope(a) => {
                output::<result::AddScope>(store.add_scope(actor, &a.description, &a.deployment))
            }
            Self::AddPrincipal(principal) => {
                output::<result::AddPrincipal>(store.add_principal(actor, &principal))
            }
            Self::AddSource(source) => {
                output::<result::AddSource>(store.add_source(actor, &source))
            }
            Self::AddComponent(component) => {
                output::<result::AddComponent>(store.add_component(actor, &component))
            }
            Self::AddPath(path) => output::<result::AddPath>(store.add_path(actor, &path)),
            Self::SetPathSources(a) => output::<result::SetPathSources>(
                store.change_path_sources(actor, a.path, &a.add, &a.remove),
            ),
            Self::AddReaches(a) => {
                output::<result::AddReaches>(store.record_reach(actor, a.path, &a.reaches))
            }
            Self::AddUnit(unit) => output::<result::AddUnit>(store.plan_unit(actor, &unit)),
            Self::AddCheck(check) => output::<result::AddCheck>(store.add_check(actor, &check)),
            Self::UnitOutcome(a) => output::<result::UnitOutcome>(
                store.record_outcome(actor, a.unit, a.outcome, &a.note, &a.open),
            ),
            Self::AddGuarantee(a) => output::<result::AddGuarantee>(store.add_guarantee(
                actor,
                a.component,
                &a.guarantee,
            )),
            Self::AddDeferred(a) => output::<result::AddDeferred>(store.defer(
                actor,
                a.subject,
                a.region.as_ref(),
                &a.reason,
            )),
            Self::SettleDeferred(a) => output::<result::SettleDeferred>(store.settle_deferred(
                actor,
                a.deferred,
                &a.guarantees,
                &a.note,
            )),
            Self::RetrySite(site) => {
                output::<result::RetrySite>(store.record_retry_site(actor, &site))
            }
            Self::AddFlaw(flaw) => output::<result::AddFlaw>(store.add_flaw(actor, &flaw)),
            Self::AddEffects(a) => {
                output::<result::AddEffects>(store.add_effects(actor, a.flaw, &a.effects))
            }
            Self::Refute(a) => {
                output::<result::Refute>(store.refute_flaw(actor, a.flaw, &a.reason))
            }
            Self::AddRelation(a) => {
                output::<result::AddRelation>(store.relate_flaws(actor, a.a, a.b, a.kind, &a.note))
            }
            Self::Undermine(a) => {
                output::<result::Undermine>(store.undermine(actor, a.flaw, a.invariant, &a.note))
            }
            Self::MergeFlaws(a) => {
                output::<result::MergeFlaws>(store.merge_flaws(actor, a.duplicate, a.into, &a.note))
            }
            Self::AddPremise(a) => {
                let premise = NewPremise {
                    kind: a.kind,
                    statement: a.statement,
                };
                output::<result::AddPremise>(store.add_premise(actor, a.on, &premise))
            }
            Self::Rely(a) => {
                output::<result::Rely>(store.rely(actor, a.candidate, a.premise, a.gates))
            }
            Self::Resolve(a) => output::<result::Resolve>(store.resolve_premise(
                actor,
                a.premise,
                a.resolution,
                &a.note,
            )),
            Self::AddEvidence(a) => {
                output::<result::AddEvidence>(store.add_evidence(actor, a.subject, &a.evidence))
            }
            Self::AddExposure(exposure) => {
                output::<result::AddExposure>(store.record_exposure(actor, &exposure))
            }
            Self::AddCandidate(candidate) => {
                output::<result::AddCandidate>(store.add_candidate(actor, &candidate))
            }
            Self::AddMembers(a) => {
                output::<result::AddMembers>(store.add_members(actor, a.candidate, &a.members))
            }
            Self::AddPaths(a) => {
                output::<result::AddPaths>(store.add_paths(actor, a.candidate, &a.paths))
            }
            Self::UpdateCandidate(a) => output::<result::UpdateCandidate>(store.update_candidate(
                actor,
                a.candidate,
                &a.metadata,
                &a.note,
            )),
            Self::MergeCandidates(a) => output::<result::MergeCandidates>(store.merge_candidates(
                actor,
                a.duplicate,
                a.into,
                &a.keep,
                &a.note,
            )),
            Self::Assess(a) => output::<result::Assess>(store.assess(
                actor,
                a.candidate,
                a.vector,
                a.confidence,
                &a.rationale,
                Assessor::Researcher,
            )),
            Self::Review(a) => output::<result::Review>(store.record_validation(
                actor,
                a.candidate,
                a.verdict,
                &a.rationale,
            )),
            Self::Reassess(a) => output::<result::Reassess>(store.assess(
                actor,
                a.candidate,
                a.vector,
                a.confidence,
                &a.rationale,
                Assessor::Reviewer,
            )),
            Self::StartPoc(a) => {
                output::<result::StartPoc>(store.start_poc(actor, a.candidate, a.validation))
            }
            Self::RecordPoc(a) => output::<result::RecordPoc>(store.record_poc(
                actor,
                a.candidate,
                a.validation,
                a.result,
                &a.note,
                &a.artifacts,
            )),
        }
    }
}

pub(super) fn register(builder: &mut ToolRegistryBuilder) -> Result<(), RegistryError> {
    super::register(
        builder,
        "research",
        "Record the run's research: scope, principals, sources, components, paths and what they reach, the review plan and its outcomes, guarantees, retry sites, deferred regions, premises, flaws with their evidence and relations, exposures, and candidates with their assessments, verdicts and tests. Result: JobView<T>: add_* and outcome, retry_site, refute, undermine, resolve, start_poc → the new id, except add_reaches, add_effects, add_members, add_paths, rely → null; add_evidence → the evidence ids; assess → {assessment, status, warnings}; verdict → ValidationAttempt; reassess → as assess; record_poc → PocAttempt; the rest → null.",
        options::<Research>(),
        |store, actor, _, research| research.execute(store, actor),
    )
}

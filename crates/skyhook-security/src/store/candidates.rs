//! Candidates: coherent security scenarios over one or more flaws, the paths they start
//! from, premises and conditional assessments. Any analysis agent may raise one at any time;
//! what it lacks becomes owed work.

use std::num::NonZeroU32;

use chrono::{DateTime, Utc};
use libsql::Row;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook::db::{Db, corrupt, enum_column, optional_enum_column, params};

use super::*;
use crate::cvss::{AttackVector, BaseMetric, Cvss40, PrivilegesRequired, Severity};

/// An existing flaw, or one recorded with the candidate.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum FlawChoice {
    Existing(FlawId),
    New(Box<NewFlaw>),
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Member {
    pub flaw: FlawChoice,
    /// The flaw's position in an ordered scenario; omit for flaws that act jointly.
    pub step: Option<u16>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewCandidate {
    pub title: Text,
    pub description: Text,
    pub cwe: Option<NonZeroU32>,
    pub members: Vec<Member>,
    #[serde(default)]
    pub paths: Vec<PathId>,
    /// Conditions the scenario needs that are not yet established, each with what it gates.
    #[serde(default)]
    pub premises: Vec<NewCondition>,
    #[serde(default)]
    pub evidence: Vec<NewEvidence>,
}

/// Descriptive fields to replace; omitted ones keep their values.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CandidateMetadata {
    pub title: Option<Text>,
    pub description: Option<Text>,
    pub cwe: Option<NonZeroU32>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct CandidateSummary {
    pub id: CandidateId,
    pub title: String,
    pub status: Status,
    pub revision: u32,
    /// The current assessment's conditional severity.
    pub severity: Option<Severity>,
    /// None until assessed at the current revision.
    pub confidence: Option<ConfidenceTier>,
    pub merged_into: Option<CandidateId>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct MemberDetails {
    pub flaw: FlawSummary,
    pub step: Option<u16>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct CandidateDetails {
    pub candidate: CandidateSummary,
    pub description: String,
    pub cwe: Option<NonZeroU32>,
    pub members: Vec<MemberDetails>,
    /// The root flaws its members trace back to through `enables` relations: candidates
    /// sharing one form that root's group, which aims for one demonstration and one report.
    pub roots: Vec<FlawId>,
    pub paths: Vec<PathId>,
    /// The sources of those paths.
    pub sources: Vec<SourceId>,
    /// Every condition it relies on: its own, those linked to it and its member flaws'.
    pub premises: Vec<Condition>,
    pub evidence: Vec<Evidence>,
    /// Each member flaw's latest decision along each of the candidate's paths.
    pub exposures: Vec<Exposure>,
    pub assessment: Option<Assessment>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CandidateFilter {
    pub status: Option<Status>,
    pub path: Option<PathId>,
    pub component: Option<ComponentId>,
    pub flaw: Option<FlawId>,
    /// Candidates in this root flaw's group.
    pub root: Option<FlawId>,
    /// At least this conditional severity.
    pub severity: Option<Severity>,
    /// At least this confidence.
    pub confidence: Option<ConfidenceTier>,
    pub cwe: Option<NonZeroU32>,
    pub operation: Option<OperationKind>,
    pub guard: Option<GuardKind>,
    pub deficiency: Option<DeficiencyKind>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Assessment {
    pub id: AssessmentId,
    pub candidate: CandidateId,
    /// The candidate revision it assessed.
    pub revision: u32,
    pub vector: Cvss40,
    pub score: f64,
    /// The impact if every premise holds.
    pub severity: Severity,
    pub confidence: Confidence,
    pub rationale: String,
    pub actor: AgentId,
    pub at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AssessmentWarning {
    NetworkSourceWithNonNetworkVector {
        source: SourceId,
    },
    UnauthenticatedSourceWithPrivileges {
        source: SourceId,
    },
    /// The vector requires more privilege than the least-trusted principal that can
    /// drive one of the candidate's paths holds.
    PrincipalBelowPrivilegesRequired {
        principal: PrincipalId,
    },
    /// Premises still open: the severity is conditional on them.
    OpenPremises {
        premises: Vec<PremiseId>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AssessmentResult {
    pub assessment: Assessment,
    pub status: Status,
    pub warnings: Vec<AssessmentWarning>,
}

/// The most privilege a principal at this trust level can be assumed to bring.
fn implied_privileges(trust: Trust) -> PrivilegesRequired {
    match trust {
        Trust::Unauthenticated => PrivilegesRequired::None,
        Trust::Authenticated => PrivilegesRequired::Low,
        Trust::Local | Trust::Privileged => PrivilegesRequired::High,
    }
}

fn rank(privileges: PrivilegesRequired) -> u8 {
    match privileges {
        PrivilegesRequired::None => 0,
        PrivilegesRequired::Low => 1,
        PrivilegesRequired::High => 2,
    }
}

macro_rules! assessment_select {
    () => {
        concat!(
            "SELECT s.id,s.candidate,s.revision,s.vector,s.confidence,s.rationale,s.at,",
            actor_columns!(),
            " FROM assessment s JOIN actor a ON a.id=s.actor WHERE "
        )
    };
}

fn assessment_row(row: &Row) -> Result<Assessment> {
    let vector: Cvss40 = row
        .get::<String>(3)?
        .parse()
        .map_err(|error| corrupt(format!("stored CVSS vector: {error}")))?;
    Ok(Assessment {
        id: AssessmentId(row.get(0)?),
        candidate: CandidateId(row.get(1)?),
        revision: row.get::<i64>(2)? as u32,
        score: vector.score(),
        severity: vector.severity(),
        vector,
        confidence: enum_column(row, 4)?,
        rationale: row.get(5)?,
        at: time(row.get(6)?)?,
        actor: actor_column(row, 7)?,
    })
}

pub(super) fn assessment(db: &Db, id: AssessmentId) -> Result<Assessment> {
    one(
        db,
        concat!(assessment_select!(), "s.id=?1"),
        id,
        assessment_row,
    )
}

/// Candidates read as `c` with their standing `st`, decoded by [`summary_row`].
/// Selects candidates `c` with their standing `st`, decoded by [`summary_row`], then any
/// further columns.
macro_rules! summary_select {
    ($($more:literal)?) => {
        concat!(
            "SELECT c.id,c.title,st.status,c.revision,st.severity,c.merged_into,st.confidence",
            $(",", $more,)?
            " FROM candidate c JOIN candidate_standing st ON st.candidate=c.id WHERE "
        )
    };
}
pub(super) use summary_select;

pub(super) fn summary_row(row: &Row) -> Result<CandidateSummary> {
    Ok(CandidateSummary {
        id: CandidateId(row.get(0)?),
        title: row.get(1)?,
        status: enum_column(row, 2)?,
        revision: row.get::<i64>(3)? as u32,
        severity: optional_enum_column(row, 4)?,
        merged_into: row.get::<Option<i64>>(5)?.map(CandidateId),
        confidence: optional_enum_column(row, 6)?,
    })
}

pub(super) fn summary(db: &Db, id: CandidateId) -> Result<CandidateSummary> {
    one(db, concat!(summary_select!(), "c.id=?1"), id, summary_row)
}

/// Refuse a candidate that is unknown or was merged into another.
pub(super) fn live(db: &Db, id: CandidateId) -> Result<()> {
    let merged = one(
        db,
        "SELECT merged_into FROM candidate WHERE id=?1",
        id,
        |row| Ok(row.get::<Option<i64>>(0)?),
    )?;
    match merged {
        Some(into) => Err(Error::Merged {
            record: Record::Candidate,
            id: id.0,
            into,
        }),
        None => Ok(()),
    }
}

pub(super) fn paths_of(db: &Db, id: CandidateId) -> Result<Vec<PathId>> {
    rows(
        db,
        "SELECT path FROM candidate_path WHERE candidate=?1 ORDER BY path",
        params![id.0],
        |row| Ok(PathId(row.get(0)?)),
    )
}

pub(super) fn roots_of(db: &Db, id: CandidateId) -> Result<Vec<FlawId>> {
    rows(
        db,
        "SELECT root FROM candidate_root WHERE candidate=?1 ORDER BY root",
        params![id.0],
        |row| Ok(FlawId(row.get(0)?)),
    )
}

/// The sources of the candidate's paths.
pub(super) fn sources_of(db: &Db, id: CandidateId) -> Result<Vec<SourceId>> {
    rows(
        db,
        "SELECT DISTINCT s.source FROM candidate_path cp JOIN path_source s ON s.path=cp.path WHERE cp.candidate=?1 ORDER BY s.source",
        params![id.0],
        |row| Ok(SourceId(row.get(0)?)),
    )
}

pub(super) fn members_of(db: &Db, id: CandidateId) -> Result<Vec<MemberDetails>> {
    rows(
        db,
        "SELECT flaw,step FROM candidate_member WHERE candidate=?1 ORDER BY step IS NULL,step,flaw",
        params![id.0],
        |row| Ok((FlawId(row.get(0)?), row.get::<Option<i64>>(1)?)),
    )?
    .into_iter()
    .map(|(flaw, step)| {
        Ok(MemberDetails {
            flaw: flaws::summary(db, flaw)?,
            step: step.map(|step| step as u16),
        })
    })
    .collect()
}

pub(super) fn details(db: &Db, id: CandidateId) -> Result<CandidateDetails> {
    let (candidate, description, cwe, assessment) = one(
        db,
        concat!(
            summary_select!("c.description,c.cwe,st.assessment"),
            "c.id=?1"
        ),
        id,
        |row| {
            Ok((
                summary_row(row)?,
                row.get::<String>(7)?,
                row.get::<Option<i64>>(8)?
                    .and_then(|cwe| NonZeroU32::new(cwe as u32)),
                row.get::<Option<i64>>(9)?.map(AssessmentId),
            ))
        },
    )?;
    let members = members_of(db, id)?;
    let paths = paths_of(db, id)?;
    let mut exposures = Vec::new();
    for member in &members {
        exposures.extend(
            flaws::exposures_of(db, member.flaw.id)?
                .into_iter()
                .filter(|exposure| paths.contains(&exposure.path)),
        );
    }
    Ok(CandidateDetails {
        description,
        cwe,
        roots: roots_of(db, id)?,
        sources: sources_of(db, id)?,
        premises: premises::of_candidate(db, id)?,
        evidence: flaws::evidence_of(db, EvidenceSubject::Candidate(id))?,
        assessment: assessment.map(|id| self::assessment(db, id)).transpose()?,
        exposures,
        members,
        paths,
        candidate,
    })
}

/// Add members, recording new flaws first; whether the scenario changed: a flaw new to
/// it, or a member moved to another step.
fn insert_members(db: &Db, actor: &AgentId, id: CandidateId, members: &[Member]) -> Result<bool> {
    if members.len() > MAX_ATTACHMENTS {
        return Err(Error::Limit(Bound::Attachments));
    }
    let mut changed = false;
    for member in members {
        let flaw = match &member.flaw {
            FlawChoice::Existing(flaw) => flaws::live(db, *flaw)?.id,
            FlawChoice::New(flaw) => flaws::insert_flaw(db, actor, flaw)?,
        };
        changed |= db.execute(
            "INSERT INTO candidate_member(candidate,flaw,step) VALUES (?1,?2,?3) ON CONFLICT DO UPDATE SET step=excluded.step WHERE step IS NOT excluded.step",
            params![id.0, flaw.0, member.step],
        )? > 0;
    }
    Ok(changed)
}

/// Whether a path was new to the candidate.
fn insert_paths(db: &Db, id: CandidateId, paths: &[PathId]) -> Result<bool> {
    if paths.len() > MAX_ATTACHMENTS {
        return Err(Error::Limit(Bound::Attachments));
    }
    let mut added = false;
    for path in paths {
        paths::revision(db, *path)?;
        added |= db.execute(
            "INSERT INTO candidate_path(candidate,path) VALUES (?1,?2) ON CONFLICT DO NOTHING",
            params![id.0, path.0],
        )? > 0;
    }
    Ok(added)
}

fn record_assessment(
    db: &Db,
    actor: &AgentId,
    candidate: CandidateId,
    vector: &Cvss40,
    confidence: Confidence,
    rationale: &Text,
) -> Result<AssessmentId> {
    let threshold = runs::run(db)?.options.threshold;
    let actor_id = actor_row(db, actor)?;
    let id = AssessmentId(db.insert(
        "INSERT INTO assessment(candidate,revision,vector,severity,shortlisted,confidence,rationale,actor,at) VALUES (?1,(SELECT revision FROM candidate WHERE id=?1),?2,?3,?4,?5,?6,?7,?8)",
        params![
            candidate.0,
            vector.to_string(),
            vector.severity(),
            vector.severity() >= threshold,
            confidence,
            rationale,
            actor_id,
            now()
        ],
    )?);
    db.execute(
        "INSERT INTO assessment_premise(assessment,premise) SELECT ?1,premise FROM \
         candidate_condition WHERE candidate=?2",
        params![id.0, candidate.0],
    )?;
    for metric in BaseMetric::ALL
        .into_iter()
        .filter(|metric| vector.at_floor(*metric))
    {
        db.execute(
            "INSERT INTO assessment_floor(assessment,metric) VALUES (?1,?2)",
            params![id.0, metric],
        )?;
    }
    Ok(id)
}

/// Who records an assessment: a researcher, or a reviewer, who may re-score a candidate
/// whose current assessment it judged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Assessor {
    Researcher,
    Reviewer,
}

impl Store {
    pub fn add_candidate(&self, actor: &AgentId, input: &NewCandidate) -> Result<CandidateId> {
        if input.members.is_empty() {
            return Err(Error::NoMembers);
        }
        self.write(|db| {
            let id = CandidateId(db.insert(
                "INSERT INTO candidate(title,description,cwe) VALUES (?1,?2,?3)",
                params![
                    &input.title,
                    &input.description,
                    input.cwe.map(NonZeroU32::get)
                ],
            )?);
            insert_members(db, actor, id, &input.members)?;
            insert_paths(db, id, &input.paths)?;
            if input.premises.len() > MAX_ATTACHMENTS {
                return Err(Error::Limit(Bound::Attachments));
            }
            for condition in &input.premises {
                match condition {
                    NewCondition::Relies { gates, premise } => {
                        premises::link(db, actor, id, *premise, *gates)?
                    }
                    NewCondition::Own {
                        gates,
                        kind,
                        statement,
                    } => {
                        let premise = NewPremise {
                            kind: *kind,
                            statement: statement.clone(),
                        };
                        let on = PremiseOn::Candidate { id, gates: *gates };
                        premises::insert(db, actor, on, &premise)?;
                    }
                }
            }
            flaws::insert_evidence(db, actor, EvidenceSubject::Candidate(id), &input.evidence)?;
            audit(
                db,
                actor,
                EventKind::Recorded,
                Detail::Note(input.title.as_str()),
                &[Subject::Candidate(id)],
            )?;
            Ok(id)
        })
    }

    /// Adds flaws to the scenario or moves them to another step, a material change;
    /// restating members changes nothing.
    pub fn add_members(&self, actor: &AgentId, id: CandidateId, members: &[Member]) -> Result<()> {
        self.write(|db| {
            live(db, id)?;
            if insert_members(db, actor, id, members)? {
                flaws::touch_candidates(db, &[id])?;
            }
            audit(
                db,
                actor,
                EventKind::Updated,
                Detail::Note("members"),
                &[Subject::Candidate(id)],
            )
            .map(drop)
        })
    }

    /// Names more paths the scenario starts from, a material change; restating known
    /// paths changes nothing.
    pub fn add_paths(&self, actor: &AgentId, id: CandidateId, paths: &[PathId]) -> Result<()> {
        self.write(|db| {
            live(db, id)?;
            if insert_paths(db, id, paths)? {
                flaws::touch_candidates(db, &[id])?;
            }
            audit(
                db,
                actor,
                EventKind::Updated,
                Detail::Note("paths"),
                &[Subject::Candidate(id)],
            )
            .map(drop)
        })
    }

    /// Descriptive edits keep every decision about the candidate.
    pub fn update_candidate(
        &self,
        actor: &AgentId,
        id: CandidateId,
        metadata: &CandidateMetadata,
        note: &Text,
    ) -> Result<()> {
        self.write(|db| {
            live(db, id)?;
            db.execute(
                "UPDATE candidate SET title=coalesce(?2,title),description=coalesce(?3,description),cwe=coalesce(?4,cwe) WHERE id=?1",
                params![
                    id.0,
                    metadata.title.as_ref(),
                    metadata.description.as_ref(),
                    metadata.cwe.map(NonZeroU32::get)
                ],
            )?;
            audit(db, actor, EventKind::Updated, Detail::Note(note.as_str()), &[Subject::Candidate(id)])
                .map(drop)
        })
    }

    /// Moves `duplicate`'s flaws, paths, evidence, the conditions it relies on, its own
    /// conditions named in `keep` and the units that name no other condition of its own to
    /// `into` and bumps `into`, so its assessment and review are owed again while its tests
    /// stand; `duplicate` keeps only its history, the conditions of its own the survivor drops
    /// and the units asking about them, ended deferred, and one finding covers both.
    pub fn merge_candidates(
        &self,
        actor: &AgentId,
        duplicate: CandidateId,
        into: CandidateId,
        keep: &[PremiseId],
        note: &Text,
    ) -> Result<()> {
        if duplicate == into {
            return Err(Error::SelfCandidate);
        }
        if keep.len() > MAX_ATTACHMENTS {
            return Err(Error::Limit(Bound::Attachments));
        }
        self.write(|db| {
            live(db, duplicate)?;
            live(db, into)?;
            for premise in keep {
                if !matches!(premises::subject(db, *premise)?,
                    PremiseOn::Candidate { id, .. } if id == duplicate)
                {
                    return Err(Error::NotItsPremise {
                        premise: *premise,
                        candidate: duplicate,
                    });
                }
                for sql in [
                    "UPDATE premise SET candidate=?2 WHERE id=?1",
                    "UPDATE candidate_premise SET candidate=?2 WHERE premise=?1",
                ] {
                    db.execute(sql, params![premise.0, into.0])?;
                }
            }
            for sql in [
                "INSERT INTO candidate_premise(candidate,premise,gate,metric) SELECT ?2,l.premise,\
                 l.gate,l.metric FROM candidate_premise l JOIN premise p ON p.id=l.premise WHERE \
                 l.candidate=?1 AND p.candidate IS NULL ON CONFLICT DO NOTHING",
                "INSERT INTO candidate_member(candidate,flaw,step) SELECT ?2,flaw,step FROM candidate_member WHERE candidate=?1 ON CONFLICT DO NOTHING",
                "INSERT INTO candidate_path(candidate,path) SELECT ?2,path FROM candidate_path WHERE candidate=?1 ON CONFLICT DO NOTHING",
                "UPDATE evidence SET candidate=?2 WHERE candidate=?1",
                "UPDATE candidate SET merged_into=?2 WHERE merged_into=?1",
                "UPDATE candidate SET merged_into=?2 WHERE id=?1",
            ] {
                db.execute(sql, params![duplicate.0, into.0])?;
            }
            plan::merge(db, actor, duplicate, into)?;
            flaws::touch_candidates(db, &[into])?;
            audit(db, actor, EventKind::Merged, Detail::Note(note.as_str()), &[Subject::Candidate(duplicate), Subject::Candidate(into)])
                .map(drop)
        })
    }

    /// Assesses the candidate at its current revision. The vector scores the impact if
    /// every premise holds; confidence carries the uncertainty, never the severity.
    /// Restating the current assessment's vector and confidence records nothing. One with
    /// the vector and conditions of an assessment a verdict judged carries that verdict, so a
    /// material change owes review again only when the assessment changes. Once a
    /// verdict judged the current assessment, only a reviewer may record a different one: it
    /// re-scores the candidate at the same revision, so the verdict no longer stands and the
    /// candidate is reviewed again while its tests stay; anyone else needs a material change.
    pub fn assess(
        &self,
        actor: &AgentId,
        id: CandidateId,
        vector: Cvss40,
        confidence: Confidence,
        rationale: &Text,
        assessor: Assessor,
    ) -> Result<AssessmentResult> {
        self.write(|db| {
            live(db, id)?;
            let standing = workflow::standing(db, id)?;
            let current = standing
                .assessment
                .map(|current| self::assessment(db, current))
                .transpose()?;
            let restated = current.as_ref().is_some_and(|current| {
                current.vector == vector && current.confidence == confidence
            });
            let assessment = match current {
                Some(current) if restated => current.id,
                Some(current)
                    if assessor == Assessor::Researcher && standing.validation.is_some() =>
                {
                    return Err(Error::AssessmentReviewed {
                        assessment: current.id,
                    });
                }
                _ => record_assessment(db, actor, id, &vector, confidence, rationale)?,
            };
            let mut sources = sources_of(db, id)?
                .into_iter()
                .map(|source| scope::source(db, source))
                .collect::<Result<Vec<_>>>()?;
            sources.sort_by_key(|source| {
                (source.kind != SourceKind::Network, source.trust, source.id)
            });
            let mut warnings = Vec::new();
            if let Some(source) = sources.first() {
                if source.kind == SourceKind::Network
                    && vector.attack_vector != AttackVector::Network
                {
                    warnings.push(AssessmentWarning::NetworkSourceWithNonNetworkVector {
                        source: source.id,
                    });
                }
                if source.trust == Trust::Unauthenticated
                    && vector.privileges_required != PrivilegesRequired::None
                {
                    warnings.push(AssessmentWarning::UnauthenticatedSourceWithPrivileges {
                        source: source.id,
                    });
                }
            }
            let mut principals = sources
                .iter()
                .flat_map(|source| source.principals.iter().copied())
                .map(|id| scope::principal(db, id))
                .collect::<Result<Vec<_>>>()?;
            principals.sort_by_key(|principal| (principal.trust, principal.id));
            if let Some(weakest) = principals.first()
                && rank(vector.privileges_required) > rank(implied_privileges(weakest.trust))
            {
                warnings.push(AssessmentWarning::PrincipalBelowPrivilegesRequired {
                    principal: weakest.id,
                });
            }
            let open = rows(
                db,
                "SELECT premise FROM candidate_condition cc WHERE candidate=?1 AND NOT EXISTS \
                 (SELECT 1 FROM premise_decision WHERE premise=cc.premise AND current) ORDER BY \
                 premise",
                params![id.0],
                |row| Ok(PremiseId(row.get(0)?)),
            )?;
            if !open.is_empty() {
                warnings.push(AssessmentWarning::OpenPremises { premises: open });
            }
            if !restated {
                audit(
                    db,
                    actor,
                    EventKind::Assessed,
                    Detail::Record(EventRecord::Assessment(assessment)),
                    &[Subject::Candidate(id)],
                )?;
            }
            Ok(AssessmentResult {
                assessment: self::assessment(db, assessment)?,
                status: summary(db, id)?.status,
                warnings,
            })
        })
    }

    pub fn assessments(&self, id: CandidateId, page: Page) -> Result<Listing<Assessment>> {
        self.read(|db| {
            let assessments = rows(
                db,
                concat!(
                    assessment_select!(),
                    "s.candidate=?1 AND s.id>?2 ORDER BY s.id LIMIT ?3"
                ),
                params![id.0, page.after, page.limit()],
                assessment_row,
            )?;
            Ok(Listing::keyset(assessments, page))
        })
    }

    pub fn candidate(&self, id: CandidateId) -> Result<CandidateDetails> {
        self.read(|db| details(db, id))
    }

    pub fn candidates(
        &self,
        filter: &CandidateFilter,
        page: Page,
    ) -> Result<Listing<CandidateSummary>> {
        self.read(|db| {
            let tiers: Vec<ConfidenceTier> = (ConfidenceTier::ALL.into_iter())
                .filter(|tier| filter.confidence.is_none_or(|least| *tier >= least))
                .collect();
            let candidates = rows(
                db,
                concat!(
                    summary_select!(),
                    "c.id>?1 AND (?3 IS NULL OR st.status=?3) \
                     AND (?4 IS NULL OR EXISTS (SELECT 1 FROM candidate_path WHERE candidate=c.id AND path=?4)) \
                     AND (?5 IS NULL OR EXISTS (SELECT 1 FROM candidate_member m JOIN flaw f ON f.id=m.flaw WHERE m.candidate=c.id AND f.component=?5)) \
                     AND (?6 IS NULL OR EXISTS (SELECT 1 FROM candidate_member WHERE candidate=c.id AND flaw=?6)) \
                     AND (?7 IS NULL OR (SELECT rank FROM severity_kind WHERE name=st.severity)>=(SELECT rank FROM severity_kind WHERE name=?7)) \
                     AND (?8 IS NULL OR c.cwe=?8) \
                     AND (?9 IS NULL OR EXISTS (SELECT 1 FROM candidate_member m JOIN flaw f ON f.id=m.flaw WHERE m.candidate=c.id AND f.operation=?9)) \
                     AND (?10 IS NULL OR EXISTS (SELECT 1 FROM candidate_member m JOIN flaw f ON f.id=m.flaw WHERE m.candidate=c.id AND f.guard=?10)) \
                     AND (?11 IS NULL OR EXISTS (SELECT 1 FROM candidate_member m JOIN flaw f ON f.id=m.flaw WHERE m.candidate=c.id AND f.deficiency=?11)) \
                     AND (?12 IS NULL OR EXISTS (SELECT 1 FROM candidate_root WHERE candidate=c.id AND root=?12)) \
                     AND (?13 IS NULL OR st.confidence IN (SELECT value FROM json_each(?14))) \
                     ORDER BY c.id LIMIT ?2"
                ),
                params![
                    page.after,
                    page.limit(),
                    filter.status,
                    filter.path.map(|id| id.0),
                    filter.component.map(|id| id.0),
                    filter.flaw.map(|id| id.0),
                    filter.severity,
                    filter.cwe.map(NonZeroU32::get),
                    filter.operation,
                    filter.guard,
                    filter.deficiency,
                    filter.root.map(|id| id.0),
                    filter.confidence,
                    json_list(&tiers)?
                ],
                summary_row,
            )?;
            Ok(Listing::keyset(candidates, page))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::tests::{
            assess, assess_at, confidence, contributes, fixture, flaw_revision, holds, member,
            new_flaw, new_premise, poc, queued, report_input, root_of, scenario, status, text,
            validated, verdict,
        },
        *,
    };

    #[test]
    fn restating_a_record_changes_nothing() {
        let (_directory, store, actor) = fixture();
        let id = validated(&store, &actor);
        let (flaw, path) = member(&store, id);
        let other = store
            .add_flaw(
                &actor,
                &new_flaw(
                    store.flaw(flaw).unwrap().record.flaw.component,
                    "second copy",
                ),
            )
            .unwrap();
        store
            .relate_flaws(
                &actor,
                flaw,
                other,
                RelationKind::SameRootCause,
                &text("one cause"),
            )
            .unwrap();
        let exposure = store.candidate(id).unwrap().exposures[0].id;
        let settled = flaw_revision(&store, flaw);

        // Restating the relation, the decision however its conditions read, an effect, a
        // path or the assessment keeps the verdict.
        store
            .relate_flaws(
                &actor,
                other,
                flaw,
                RelationKind::SameRootCause,
                &text("restated"),
            )
            .unwrap();
        contributes(&store, &actor, flaw, path);
        assert_eq!(store.candidate(id).unwrap().exposures[0].id, exposure);
        let decide = |outcome, conditions: &str| {
            store
                .record_exposure(
                    &actor,
                    &NewExposure {
                        flaw,
                        path,
                        outcome,
                        conditions: text(conditions),
                        note: text("traced again"),
                        route: Vec::new(),
                    },
                )
                .unwrap()
        };
        decide(ExposureOutcome::Contributes, "any caller sets the length");
        store
            .add_effects(&actor, flaw, &new_flaw(ComponentId(1), "x").effects)
            .unwrap();
        store.add_paths(&actor, id, &[path]).unwrap();
        assess(&store, &actor, id);
        assert_eq!(flaw_revision(&store, flaw), settled);
        assert_eq!(status(&store, id), Status::Validated);

        // A different assessment needs a material change first.
        let local = "CVSS:4.0/AV:L/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N";
        assert!(matches!(
            assess_at(&store, &actor, id, local, Assessor::Researcher),
            Err(Error::AssessmentReviewed { .. })
        ));

        // Restating a resolution keeps it and the candidate's revision.
        let premise = store
            .add_premise(
                &actor,
                scenario(id),
                &new_premise(InvariantKind::Bounds, "the tail exceeds 7 bytes"),
            )
            .unwrap();
        let first = holds(&store, &actor, premise);
        let settled = store.candidate(id).unwrap().candidate.revision;
        assert_eq!(holds(&store, &actor, premise), first);
        assert_eq!(store.candidate(id).unwrap().candidate.revision, settled);

        // A new outcome is material.
        decide(ExposureOutcome::NoConsequence, "the length is bounded");
        assert_eq!(status(&store, id), Status::Identified);
    }

    /// A reviewer re-scores a candidate at its revision: the verdict no longer stands while
    /// its test does, and once the new score is reviewed the root owes a report again;
    /// anyone else needs a material change first.
    #[test]
    fn a_reviewer_rescoring_keeps_the_test_and_reopens_review() {
        let (_directory, store, actor) = fixture();
        let id = validated(&store, &actor);
        let root = root_of(&store, id);
        poc(&store, &actor, id, PocResult::Confirmed);
        let adjacent = "CVSS:4.0/AV:A/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N";
        let rescore = |assessor| assess_at(&store, &actor, id, adjacent, assessor);
        assert!(matches!(
            rescore(Assessor::Researcher),
            Err(Error::AssessmentReviewed { .. })
        ));
        let published = store.report_generation(root).unwrap().key;
        store
            .record_finding(&actor, &report_input(root), &published, &[])
            .unwrap();
        assert!(queued(&store, Phase::Report).is_empty());
        rescore(Assessor::Reviewer).unwrap();
        assert_ne!(status(&store, id), Status::Validated);
        verdict(&store, &actor, id, Verdict::Validated);
        assert_eq!(confidence(&store, id), Some(ConfidenceTier::Demonstrated));
        assert_eq!(queued(&store, Phase::Report), [WorkSubject::Root(root)]);
    }
}

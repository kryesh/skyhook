//! Flaws, the evidence about research records, the relations between flaws, and each
//! path's decision about each flaw. Material changes bump revisions here.

use chrono::{DateTime, Utc};
use libsql::Row;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook::db::{Db, enum_column, optional_enum_column, params};

use super::*;

/// The record a piece of evidence is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "id",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum EvidenceSubject {
    Flaw(FlawId),
    Candidate(CandidateId),
    Exposure(ExposureId),
    Premise(PremiseId),
    Guarantee(GuaranteeId),
    /// Contradicting evidence on a refutation lifts it.
    Refutation(RefutationId),
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewEvidence {
    pub location: Location,
    pub role: EvidenceRole,
    pub stance: Stance,
    pub note: Text,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Evidence {
    pub id: EvidenceId,
    pub location: Location,
    pub role: EvidenceRole,
    pub stance: Stance,
    pub note: String,
    pub actor: AgentId,
    pub at: DateTime<Utc>,
}

/// The guard a flaw's operation needs and how it fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GuardDeficiency {
    pub guard: GuardKind,
    pub deficiency: DeficiencyKind,
}

/// A guard and deficiency column pair from `offset`, set together or not at all.
pub(super) fn deficiency_columns(row: &Row, offset: i32) -> Result<Option<GuardDeficiency>> {
    let guard: Option<GuardKind> = optional_enum_column(row, offset)?;
    let deficiency: Option<DeficiencyKind> = optional_enum_column(row, offset + 1)?;
    Ok(guard
        .zip(deficiency)
        .map(|(guard, deficiency)| GuardDeficiency { guard, deficiency }))
}

/// A possible effect, not a settled impact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Effect {
    pub property: Property,
    pub note: Text,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewFlaw {
    pub component: ComponentId,
    pub title: Text,
    pub statement: Text,
    pub operation: OperationKind,
    pub deficiency: Option<GuardDeficiency>,
    /// What the flaw makes its code report for the wrong reason (a status, an error, or
    /// state left for others), when it does: recording one plans a follow-up tracing that
    /// value to every consumer.
    #[serde(default)]
    pub misreports: Option<Misreport>,
    #[serde(default)]
    pub effects: Vec<Effect>,
    #[serde(default)]
    pub evidence: Vec<NewEvidence>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FlawSummary {
    pub id: FlawId,
    pub component: ComponentId,
    pub title: String,
    pub operation: OperationKind,
    pub misreports: Option<Misreport>,
    pub revision: u32,
    /// A refutation of it stands.
    pub refuted: bool,
    pub merged_into: Option<FlawId>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Refutation {
    pub id: RefutationId,
    pub reason: String,
    /// Evidence contradicting the refutation lifted it.
    pub lifted: bool,
    pub evidence: Vec<Evidence>,
    pub actor: AgentId,
    pub at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Relation {
    pub id: RelationId,
    pub a: FlawId,
    pub b: FlawId,
    pub kind: RelationKind,
    pub note: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RouteStep {
    pub location: Location,
    pub basis: RouteBasis,
    pub note: Text,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewExposure {
    pub flaw: FlawId,
    pub path: PathId,
    pub outcome: ExposureOutcome,
    /// What the path's sources must control or establish: configuration, call order, state.
    pub conditions: Text,
    pub note: Text,
    /// The significant calls and state transitions from the entry to the flaw.
    #[serde(default)]
    pub route: Vec<RouteStep>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Exposure {
    pub id: ExposureId,
    pub flaw: FlawId,
    pub path: PathId,
    /// The flaw revision it decided; stale once the flaw changes.
    pub revision: u32,
    /// The path revision it decided; stale once the path's membership changes.
    pub path_revision: u32,
    /// Whether it still stands: made at the flaw's and the path's current revisions and
    /// contradicted by no evidence since.
    pub current: bool,
    pub outcome: ExposureOutcome,
    pub conditions: String,
    pub note: String,
    pub route: Vec<RouteStep>,
    pub evidence: Vec<Evidence>,
    pub actor: AgentId,
    pub at: DateTime<Utc>,
}

/// A flaw as recorded, apart from what the rest of the run says about it.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FlawRecord {
    pub flaw: FlawSummary,
    pub statement: String,
    pub deficiency: Option<GuardDeficiency>,
    pub effects: Vec<Effect>,
    pub evidence: Vec<Evidence>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FlawDetails {
    #[serde(flatten)]
    pub record: FlawRecord,
    pub relations: Vec<Relation>,
    pub refutation: Option<Refutation>,
    /// The latest decision of each path.
    pub exposures: Vec<Exposure>,
    /// The guarantees and premises it breaks.
    pub undermines: Vec<Invariant>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FlawFilter {
    pub component: Option<ComponentId>,
    /// Flaws in components this path reaches.
    pub path: Option<PathId>,
    pub operation: Option<OperationKind>,
}

macro_rules! flaw_select {
    () => {
        "SELECT f.id,f.component,f.title,f.operation,f.revision,f.refuted,f.merged_into,\
         f.statement,f.guard,f.deficiency,f.misreports FROM flaw_state f WHERE "
    };
}

fn flaw_row(row: &Row) -> Result<FlawSummary> {
    Ok(FlawSummary {
        id: FlawId(row.get(0)?),
        component: ComponentId(row.get(1)?),
        title: row.get(2)?,
        operation: enum_column(row, 3)?,
        misreports: optional_enum_column(row, 10)?,
        revision: row.get::<i64>(4)? as u32,
        refuted: row.get::<i64>(5)? != 0,
        merged_into: row.get::<Option<i64>>(6)?.map(FlawId),
    })
}

pub(super) fn summary(db: &Db, id: FlawId) -> Result<FlawSummary> {
    one(db, concat!(flaw_select!(), "f.id=?1"), id, flaw_row)
}

/// The flaw, refused if it was merged into another.
pub(super) fn live(db: &Db, id: FlawId) -> Result<FlawSummary> {
    let flaw = summary(db, id)?;
    match flaw.merged_into {
        Some(into) => Err(Error::Merged {
            record: Record::Flaw,
            id: id.0,
            into: into.0,
        }),
        None => Ok(flaw),
    }
}

pub(super) fn of_component(db: &Db, component: ComponentId) -> Result<Vec<FlawSummary>> {
    rows(
        db,
        concat!(flaw_select!(), "f.component=?1 ORDER BY f.id"),
        params![component.0],
        flaw_row,
    )
}

/// A material change to `flaws`: every decision about them, and about the candidates
/// built on them, is owed again.
pub(super) fn touch_flaws(db: &Db, flaws: &[FlawId]) -> Result<()> {
    let ids = json_list(&flaws.iter().map(|id| id.0).collect::<Vec<_>>())?;
    db.execute(
        "UPDATE flaw SET revision=revision+1 WHERE id IN (SELECT value FROM json_each(?1))",
        params![&ids],
    )?;
    db.execute(
        "UPDATE candidate SET revision=revision+1 WHERE id IN (SELECT candidate FROM candidate_member WHERE flaw IN (SELECT value FROM json_each(?1)))",
        params![&ids],
    )?;
    Ok(())
}

/// The candidates built on `flaw`.
fn candidates_of(db: &Db, flaw: FlawId) -> Result<Vec<CandidateId>> {
    rows(
        db,
        "SELECT candidate FROM candidate_member WHERE flaw=?1",
        params![flaw.0],
        |row| Ok(CandidateId(row.get(0)?)),
    )
}

/// A material change to `candidates`: their assessments and verdicts are owed again.
pub(super) fn touch_candidates(db: &Db, candidates: &[CandidateId]) -> Result<()> {
    let ids = json_list(&candidates.iter().map(|id| id.0).collect::<Vec<_>>())?;
    db.execute(
        "UPDATE candidate SET revision=revision+1 WHERE id IN (SELECT value FROM json_each(?1))",
        params![&ids],
    )?;
    Ok(())
}

/// A contradiction of `candidates`' scenarios: the tests they hold no longer stand. Only a
/// refuted member and a condition gating the scenario that fails or is undermined
/// contradict it; other changes leave tests alone.
pub(super) fn contradict(db: &Db, candidates: &[CandidateId]) -> Result<()> {
    let ids = json_list(&candidates.iter().map(|id| id.0).collect::<Vec<_>>())?;
    db.execute(
        "UPDATE candidate SET contradictions=contradictions+1 WHERE id IN (SELECT value FROM json_each(?1))",
        params![&ids],
    )?;
    Ok(())
}

/// The candidates that build on `flaw` along `path`.
fn exposed_candidates(db: &Db, flaw: FlawId, path: PathId) -> Result<Vec<CandidateId>> {
    rows(
        db,
        "SELECT m.candidate FROM candidate_member m JOIN candidate_path p ON p.candidate=m.candidate AND p.path=?2 WHERE m.flaw=?1",
        params![flaw.0, path.0],
        |row| Ok(CandidateId(row.get(0)?)),
    )
}

macro_rules! evidence_select {
    () => {
        concat!(
            "SELECT e.id,e.role,e.stance,e.note,e.at,",
            actor_columns!(),
            ",",
            scope::location_columns!("l"),
            " FROM evidence e JOIN actor a ON a.id=e.actor JOIN location l ON l.id=e.location WHERE "
        )
    };
}

fn evidence_row(row: &Row) -> Result<Evidence> {
    Ok(Evidence {
        id: EvidenceId(row.get(0)?),
        role: enum_column(row, 1)?,
        stance: enum_column(row, 2)?,
        note: row.get(3)?,
        at: time(row.get(4)?)?,
        actor: actor_column(row, 5)?,
        location: scope::location_column(row, 6)?,
    })
}

pub(super) fn evidence_of(db: &Db, subject: EvidenceSubject) -> Result<Vec<Evidence>> {
    let (sql, id): (&'static str, i64) = match subject {
        EvidenceSubject::Flaw(id) => (concat!(evidence_select!(), "e.flaw=?1 ORDER BY e.id"), id.0),
        EvidenceSubject::Candidate(id) => (
            concat!(evidence_select!(), "e.candidate=?1 ORDER BY e.id"),
            id.0,
        ),
        EvidenceSubject::Exposure(id) => (
            concat!(evidence_select!(), "e.exposure=?1 ORDER BY e.id"),
            id.0,
        ),
        EvidenceSubject::Premise(id) => (
            concat!(evidence_select!(), "e.premise=?1 ORDER BY e.id"),
            id.0,
        ),
        EvidenceSubject::Guarantee(id) => (
            concat!(evidence_select!(), "e.guarantee=?1 ORDER BY e.id"),
            id.0,
        ),
        EvidenceSubject::Refutation(id) => (
            concat!(evidence_select!(), "e.refutation=?1 ORDER BY e.id"),
            id.0,
        ),
    };
    rows(db, sql, params![id], evidence_row)
}

fn refutation_flaw(db: &Db, id: RefutationId) -> Result<FlawId> {
    scope::known(db, id)?;
    one(
        db,
        "SELECT flaw FROM flaw_refutation WHERE id=?1",
        id,
        |row| Ok(FlawId(row.get(0)?)),
    )
}

/// Insert evidence about `subject`. Contradicting evidence is a material change to
/// whatever the subject's decisions rest on.
pub(super) fn insert_evidence(
    db: &Db,
    actor: &AgentId,
    subject: EvidenceSubject,
    evidence: &[NewEvidence],
) -> Result<Vec<EvidenceId>> {
    if evidence.len() > MAX_ATTACHMENTS {
        return Err(Error::Limit(Bound::Attachments));
    }
    let mut columns = [None; 6];
    let subjects = match subject {
        EvidenceSubject::Flaw(id) => {
            live(db, id)?;
            columns[0] = Some(id.0);
            vec![Subject::Flaw(id)]
        }
        EvidenceSubject::Candidate(id) => {
            candidates::live(db, id)?;
            columns[1] = Some(id.0);
            vec![Subject::Candidate(id)]
        }
        EvidenceSubject::Exposure(id) => {
            columns[2] = Some(id.0);
            vec![Subject::Flaw(exposure_pair(db, id)?.0)]
        }
        EvidenceSubject::Premise(id) => {
            columns[3] = Some(id.0);
            premises::subject(db, id)?.subject().into_iter().collect()
        }
        EvidenceSubject::Guarantee(id) => {
            scope::known(db, id)?;
            columns[4] = Some(id.0);
            Vec::new()
        }
        EvidenceSubject::Refutation(id) => {
            columns[5] = Some(id.0);
            vec![Subject::Flaw(refutation_flaw(db, id)?)]
        }
    };
    if evidence
        .iter()
        .any(|evidence| evidence.stance == Stance::Contradicts)
    {
        match subject {
            EvidenceSubject::Flaw(id) => touch_flaws(db, &[id])?,
            EvidenceSubject::Candidate(id) => touch_candidates(db, &[id])?,
            EvidenceSubject::Exposure(id) => {
                let (flaw, path) = exposure_pair(db, id)?;
                touch_candidates(db, &exposed_candidates(db, flaw, path)?)?;
            }
            EvidenceSubject::Premise(id) => {
                premises::touch(db, actor, id)?;
            }
            EvidenceSubject::Guarantee(_) => {}
            // Lifting a refutation returns the flaw to every decision about it.
            EvidenceSubject::Refutation(id) => touch_flaws(db, &[refutation_flaw(db, id)?])?,
        }
    }
    let actor_id = actor_row(db, actor)?;
    let [flaw, candidate, exposure, premise, guarantee, refutation] = columns;
    let mut ids = Vec::with_capacity(evidence.len());
    for evidence in evidence {
        let location = scope::location_id(db, &evidence.location)?;
        let id = EvidenceId(db.insert(
            "INSERT INTO evidence(flaw,candidate,exposure,premise,guarantee,refutation,location,role,stance,note,actor,at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![flaw, candidate, exposure, premise, guarantee, refutation, location.0, evidence.role, evidence.stance, &evidence.note, actor_id, now()],
        )?);
        audit(
            db,
            actor,
            EventKind::Recorded,
            Detail::Record(EventRecord::Evidence(id)),
            &subjects,
        )?;
        ids.push(id);
    }
    Ok(ids)
}

fn exposure_pair(db: &Db, id: ExposureId) -> Result<(FlawId, PathId)> {
    one(
        db,
        "SELECT flaw,path FROM exposure WHERE id=?1",
        id,
        |row| Ok((FlawId(row.get(0)?), PathId(row.get(1)?))),
    )
}

/// Record possible effects, restating the note of a known property; whether a property
/// was new to the flaw.
fn insert_effects(db: &Db, flaw: FlawId, effects: &[Effect]) -> Result<bool> {
    let mut added = false;
    for effect in effects {
        added |= db.execute(
            "INSERT INTO flaw_effect(flaw,property,note) VALUES (?1,?2,?3) ON CONFLICT DO NOTHING",
            params![flaw.0, effect.property, &effect.note],
        )? > 0;
        db.execute(
            "UPDATE flaw_effect SET note=?3 WHERE flaw=?1 AND property=?2",
            params![flaw.0, effect.property, &effect.note],
        )?;
    }
    Ok(added)
}

pub(super) fn insert_flaw(db: &Db, actor: &AgentId, input: &NewFlaw) -> Result<FlawId> {
    scope::known(db, input.component)?;
    let id = FlawId(db.insert(
        "INSERT INTO flaw(component,title,statement,operation,guard,deficiency,misreports) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![
            input.component.0,
            &input.title,
            &input.statement,
            input.operation,
            input.deficiency.map(|d| d.guard),
            input.deficiency.map(|d| d.deficiency),
            input.misreports
        ],
    )?);
    insert_effects(db, id, &input.effects)?;
    audit(
        db,
        actor,
        EventKind::Recorded,
        Detail::Note(input.title.as_str()),
        &[Subject::Flaw(id)],
    )?;
    insert_evidence(db, actor, EvidenceSubject::Flaw(id), &input.evidence)?;
    if let Some(misreport) = input.misreports {
        consequences::trace(db, actor, id, misreport)?;
    }
    Ok(id)
}

macro_rules! exposure_select {
    () => {
        concat!(
            "SELECT x.id,x.flaw,x.path,x.revision,x.path_revision,x.standing,x.outcome,\
             x.conditions,x.note,x.at,",
            actor_columns!(),
            " FROM exposure_decision x JOIN flaw f ON f.id=x.flaw JOIN actor a ON \
             a.id=x.actor WHERE "
        )
    };
}

impl Exposure {
    /// Whether `input` says exactly what this decision says.
    fn restates(&self, input: &NewExposure) -> bool {
        self.outcome == input.outcome
            && self.conditions == input.conditions.as_str()
            && self.note == input.note.as_str()
            && self.route.len() == input.route.len()
            && (self.route.iter().zip(&input.route)).all(|(step, new)| {
                step.location == new.location && step.basis == new.basis && step.note == new.note
            })
    }
}

fn exposure_row(row: &Row) -> Result<Exposure> {
    Ok(Exposure {
        id: ExposureId(row.get(0)?),
        flaw: FlawId(row.get(1)?),
        path: PathId(row.get(2)?),
        revision: row.get::<i64>(3)? as u32,
        path_revision: row.get::<i64>(4)? as u32,
        current: row.get::<i64>(5)? != 0,
        outcome: enum_column(row, 6)?,
        conditions: row.get(7)?,
        note: row.get(8)?,
        at: time(row.get(9)?)?,
        actor: actor_column(row, 10)?,
        route: Vec::new(),
        evidence: Vec::new(),
    })
}

fn hydrate(db: &Db, mut exposure: Exposure) -> Result<Exposure> {
    exposure.route = rows(
        db,
        concat!(
            "SELECT s.basis,s.note,",
            scope::location_columns!("l"),
            " FROM exposure_step s JOIN location l ON l.id=s.location WHERE s.exposure=?1 ORDER BY s.position"
        ),
        params![exposure.id.0],
        |row| {
            Ok(RouteStep {
                basis: enum_column(row, 0)?,
                note: row.get::<String>(1)?.parse()?,
                location: scope::location_column(row, 2)?,
            })
        },
    )?;
    exposure.evidence = evidence_of(db, EvidenceSubject::Exposure(exposure.id))?;
    Ok(exposure)
}

/// The latest decision of each path about `flaw`.
pub(super) fn exposures_of(db: &Db, flaw: FlawId) -> Result<Vec<Exposure>> {
    rows(
        db,
        concat!(exposure_select!(), "x.flaw=?1 ORDER BY x.path"),
        params![flaw.0],
        exposure_row,
    )?
    .into_iter()
    .map(|exposure| hydrate(db, exposure))
    .collect()
}

pub(super) fn record(db: &Db, id: FlawId) -> Result<FlawRecord> {
    let (flaw, statement, deficiency) = one(db, concat!(flaw_select!(), "f.id=?1"), id, |row| {
        Ok((
            flaw_row(row)?,
            row.get::<String>(7)?,
            deficiency_columns(row, 8)?,
        ))
    })?;
    Ok(FlawRecord {
        flaw,
        statement,
        deficiency,
        effects: rows(
            db,
            "SELECT property,note FROM flaw_effect WHERE flaw=?1 ORDER BY property",
            params![id.0],
            |row| {
                Ok(Effect {
                    property: enum_column(row, 0)?,
                    note: row.get::<String>(1)?.parse()?,
                })
            },
        )?,
        evidence: evidence_of(db, EvidenceSubject::Flaw(id))?,
    })
}

fn relation_row(row: &Row) -> Result<Relation> {
    Ok(Relation {
        id: RelationId(row.get(0)?),
        a: FlawId(row.get(1)?),
        b: FlawId(row.get(2)?),
        kind: enum_column(row, 3)?,
        note: row.get(4)?,
    })
}

pub(super) fn details(db: &Db, id: FlawId) -> Result<FlawDetails> {
    Ok(FlawDetails {
        record: record(db, id)?,
        relations: rows(
            db,
            "SELECT id,a,b,kind,note FROM relation WHERE a=?1 OR b=?1 ORDER BY id",
            params![id.0],
            relation_row,
        )?,
        refutation: optional(
            db,
            concat!(
                "SELECT r.id,r.reason,EXISTS (SELECT 1 FROM evidence v WHERE v.refutation=r.id \
                 AND v.stance='contradicts'),r.at,",
                actor_columns!(),
                " FROM flaw_refutation r JOIN actor a ON a.id=r.actor WHERE r.flaw=?1 ORDER BY r.id DESC LIMIT 1"
            ),
            params![id.0],
            |row| {
                Ok(Refutation {
                    id: RefutationId(row.get(0)?),
                    reason: row.get(1)?,
                    lifted: row.get::<i64>(2)? != 0,
                    evidence: Vec::new(),
                    at: time(row.get(3)?)?,
                    actor: actor_column(row, 4)?,
                })
            },
        )?
        .map(|mut refutation| {
            refutation.evidence =
                evidence_of(db, EvidenceSubject::Refutation(refutation.id))?;
            Ok::<_, Error>(refutation)
        })
        .transpose()?,
        exposures: exposures_of(db, id)?,
        undermines: consequences::undermined_by_flaw(db, id)?,
    })
}

impl Store {
    pub fn add_flaw(&self, actor: &AgentId, input: &NewFlaw) -> Result<FlawId> {
        self.write(|db| insert_flaw(db, actor, input))
    }

    /// Adds or restates possible effects; a property new to the flaw is a materially
    /// different consequence.
    pub fn add_effects(&self, actor: &AgentId, flaw: FlawId, effects: &[Effect]) -> Result<()> {
        self.write(|db| {
            live(db, flaw)?;
            if insert_effects(db, flaw, effects)? {
                touch_flaws(db, &[flaw])?;
            }
            audit(
                db,
                actor,
                EventKind::Updated,
                Detail::Note("effects"),
                &[Subject::Flaw(flaw)],
            )
            .map(drop)
        })
    }

    /// The flaw is not a defect, for example because another component enforces the
    /// contract. Its candidates are owed reassessment and their tests no longer stand; only
    /// evidence contradicting the refutation lifts it.
    pub fn refute_flaw(
        &self,
        actor: &AgentId,
        flaw: FlawId,
        reason: &Text,
    ) -> Result<RefutationId> {
        self.write(|db| {
            live(db, flaw)?;
            let actor_id = actor_row(db, actor)?;
            let id = RefutationId(db.insert(
                "INSERT INTO flaw_refutation(flaw,reason,actor,at) VALUES (?1,?2,?3,?4)",
                params![flaw.0, reason, actor_id, now()],
            )?);
            let candidates = candidates_of(db, flaw)?;
            touch_candidates(db, &candidates)?;
            contradict(db, &candidates)?;
            audit(
                db,
                actor,
                EventKind::Refuted,
                Detail::Record(EventRecord::Refutation(id)),
                &[Subject::Flaw(flaw)],
            )?;
            Ok(id)
        })
    }

    /// Relates two flaws: a new relation bears on how they compose, so it is a material
    /// change to the candidates built on both, not to the flaws; restating one only
    /// replaces its note.
    pub fn relate_flaws(
        &self,
        actor: &AgentId,
        a: FlawId,
        b: FlawId,
        kind: RelationKind,
        note: &Text,
    ) -> Result<RelationId> {
        if a == b {
            return Err(Error::SelfFlaw);
        }
        self.write(|db| {
            live(db, a)?;
            live(db, b)?;
            let (a, b) = if kind.symmetric() && b < a {
                (b, a)
            } else {
                (a, b)
            };
            let added = db.execute(
                "INSERT INTO relation(a,b,kind,note) VALUES (?1,?2,?3,?4) ON CONFLICT DO NOTHING",
                params![a.0, b.0, kind, note],
            )? > 0;
            db.execute(
                "UPDATE relation SET note=?4 WHERE a=?1 AND b=?2 AND kind=?3",
                params![a.0, b.0, kind, note],
            )?;
            let id = stored(
                db,
                "SELECT id FROM relation WHERE a=?1 AND b=?2 AND kind=?3",
                params![a.0, b.0, kind],
                |row| Ok(RelationId(row.get(0)?)),
            )?;
            if added {
                let others = candidates_of(db, b)?;
                let shared: Vec<_> = (candidates_of(db, a)?.into_iter())
                    .filter(|candidate| others.contains(candidate))
                    .collect();
                touch_candidates(db, &shared)?;
            }
            audit(
                db,
                actor,
                EventKind::Recorded,
                Detail::Record(EventRecord::Relation(id)),
                &[Subject::Flaw(a), Subject::Flaw(b)],
            )?;
            Ok(id)
        })
    }

    /// Moves everything recorded about `duplicate` to `into` and moves `into` past both
    /// revisions, so every decision either had is owed again; `duplicate` keeps only its
    /// history. Relations move to `into` unless it already has them or they would relate
    /// it to itself; those stay with the duplicate as history.
    pub fn merge_flaws(
        &self,
        actor: &AgentId,
        duplicate: FlawId,
        into: FlawId,
        note: &Text,
    ) -> Result<()> {
        if duplicate == into {
            return Err(Error::SelfFlaw);
        }
        self.write(|db| {
            live(db, duplicate)?;
            live(db, into)?;
            let moved = params![duplicate.0, into.0];
            for sql in [
                "UPDATE evidence SET flaw=?2 WHERE flaw=?1",
                "UPDATE exposure SET flaw=?2 WHERE flaw=?1",
                "UPDATE premise_resolution SET flaw=?2 WHERE flaw=?1",
                "UPDATE premise SET flaw=?2 WHERE flaw=?1",
                "UPDATE flaw_refutation SET flaw=?2 WHERE flaw=?1",
                "UPDATE plan_unit SET flaw=?2 WHERE flaw=?1",
                "UPDATE undermining SET flaw=?2 WHERE flaw=?1 AND NOT EXISTS (SELECT 1 FROM undermining o WHERE o.flaw=?2 AND o.guarantee IS undermining.guarantee AND o.premise IS undermining.premise)",
                "UPDATE flaw SET misreports=coalesce(misreports,(SELECT misreports FROM flaw WHERE id=?1)) WHERE id=?2",
                "INSERT INTO flaw_effect(flaw,property,note) SELECT ?2,property,note FROM flaw_effect WHERE flaw=?1 ON CONFLICT DO NOTHING",
                "INSERT INTO candidate_member(candidate,flaw,step) SELECT candidate,?2,step FROM candidate_member WHERE flaw=?1 ON CONFLICT DO NOTHING",
                "DELETE FROM candidate_member WHERE flaw=?1",
                "UPDATE flaw SET merged_into=?2 WHERE merged_into=?1",
                "UPDATE flaw SET merged_into=?2 WHERE id=?1",
                "UPDATE flaw SET revision=max(revision,(SELECT revision FROM flaw WHERE id=?1)) WHERE id=?2",
            ] {
                db.execute(sql, moved.clone())?;
            }
            let relations = rows(
                db,
                "SELECT id,a,b,kind FROM relation WHERE a=?1 OR b=?1",
                params![duplicate.0],
                |row| {
                    Ok((
                        RelationId(row.get(0)?),
                        FlawId(row.get(1)?),
                        FlawId(row.get(2)?),
                        enum_column::<RelationKind>(row, 3)?,
                    ))
                },
            )?;
            let canonical = |flaw| if flaw == duplicate { into } else { flaw };
            for (id, a, b, kind) in relations {
                let (a, b) = (canonical(a), canonical(b));
                if a == b {
                    continue;
                }
                let (a, b) = if kind.symmetric() && b < a { (b, a) } else { (a, b) };
                db.execute(
                    "UPDATE relation SET a=?2,b=?3 WHERE id=?1 AND NOT EXISTS (SELECT 1 FROM relation WHERE a=?2 AND b=?3 AND kind=?4)",
                    params![id.0, a.0, b.0, kind],
                )?;
            }
            touch_flaws(db, &[into])?;
            audit(db, actor, EventKind::Merged, Detail::Note(note.as_str()), &[Subject::Flaw(duplicate), Subject::Flaw(into)])
                .map(drop)
        })
    }

    /// Records `path`'s decision about a flaw at the flaw's and the path's current
    /// revisions. Restating the standing decision word for word records nothing; only an
    /// outcome other than the pair's latest one is a material change to the candidates built
    /// on it.
    pub fn record_exposure(&self, actor: &AgentId, input: &NewExposure) -> Result<ExposureId> {
        if input.route.len() > MAX_ATTACHMENTS {
            return Err(Error::Limit(Bound::RouteSteps));
        }
        self.write(|db| {
            let flaw = live(db, input.flaw)?;
            let path_revision = paths::revision(db, input.path)?;
            let latest = exposures_of(db, input.flaw)?
                .into_iter()
                .find(|exposure| exposure.path == input.path);
            if let Some(latest) = &latest
                && latest.current
                && latest.restates(input)
            {
                return Ok(latest.id);
            }
            // Only a changed outcome changes what the pair contributes to a scenario; a
            // re-decision of a stale pair with the same outcome refreshes it without one.
            if latest.is_none_or(|latest| latest.outcome != input.outcome) {
                touch_candidates(db, &exposed_candidates(db, input.flaw, input.path)?)?;
            }
            let actor_id = actor_row(db, actor)?;
            let id = ExposureId(db.insert(
                "INSERT INTO exposure(flaw,path,revision,path_revision,outcome,conditions,note,actor,at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![input.flaw.0, input.path.0, flaw.revision, path_revision, input.outcome, &input.conditions, &input.note, actor_id, now()],
            )?);
            for (position, step) in input.route.iter().enumerate() {
                let location = scope::location_id(db, &step.location)?;
                db.execute(
                    "INSERT INTO exposure_step(exposure,position,location,basis,note) VALUES (?1,?2,?3,?4,?5)",
                    params![id.0, position, location.0, step.basis, &step.note],
                )?;
            }
            audit(db, actor, EventKind::Recorded, Detail::Record(EventRecord::Exposure(id)), &[Subject::Flaw(input.flaw)])?;
            Ok(id)
        })
    }

    pub fn add_evidence(
        &self,
        actor: &AgentId,
        subject: EvidenceSubject,
        evidence: &[NewEvidence],
    ) -> Result<Vec<EvidenceId>> {
        self.write(|db| insert_evidence(db, actor, subject, evidence))
    }

    pub fn flaw(&self, id: FlawId) -> Result<FlawDetails> {
        self.read(|db| details(db, id))
    }

    pub fn flaws(&self, filter: &FlawFilter, page: Page) -> Result<Listing<FlawSummary>> {
        self.read(|db| {
            let flaws = rows(
                db,
                concat!(
                    flaw_select!(),
                    "f.id>?1 AND (?3 IS NULL OR f.component=?3) AND (?4 IS NULL OR f.component IN \
                     (SELECT component FROM path_reach WHERE path=?4)) AND (?5 IS NULL OR f.operation=?5) \
                     ORDER BY f.id LIMIT ?2"
                ),
                params![
                    page.after,
                    page.limit(),
                    filter.component.map(|id| id.0),
                    filter.path.map(|id| id.0),
                    filter.operation
                ],
                flaw_row,
            )?;
            Ok(Listing::keyset(flaws, page))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::tests::{
            assess, candidate, component, contradiction, contributes, fixture, new_flaw, path,
            shortlisted, status, text,
        },
        *,
    };

    /// A cycle of `enables` roots at its own lowest flaw, so the flaw it feeds joins its
    /// group rather than rooting one of its own, beside the group of a flaw nothing enables
    /// that feeds it too.
    #[test]
    fn a_cycle_and_the_flaw_it_enables_share_one_root() {
        let (_directory, store, actor) = fixture();
        let (downstream, fed, _) = candidate(&store, &actor, "fed");
        let (cyclic, low, _) = candidate(&store, &actor, "low");
        let (_, high, _) = candidate(&store, &actor, "high");
        let (_, top, _) = candidate(&store, &actor, "top");
        for (a, b) in [(low, high), (high, low), (low, fed), (top, fed)] {
            store
                .relate_flaws(&actor, a, b, RelationKind::Enables, &text("feeds it"))
                .unwrap();
        }
        let roots = |id| store.candidate(id).unwrap().roots;
        assert_eq!(
            [roots(downstream), roots(cyclic)],
            [vec![low, top], vec![low]]
        );
    }

    /// A relation bears on how flaws compose: the candidates built on both are owed
    /// reassessment, one built on only one of them is not, and every path's decision about
    /// the flaws stands.
    #[test]
    fn a_relation_reopens_candidates_and_not_flaws() {
        let (_directory, store, actor) = fixture();
        let id = shortlisted(&store, &actor);
        let recorded = store.candidate(id).unwrap();
        let (flaw, path) = (recorded.members[0].flaw.clone(), recorded.paths[0]);
        let other = store
            .add_flaw(&actor, &new_flaw(flaw.component, "second copy"))
            .unwrap();
        store
            .relate_flaws(
                &actor,
                other,
                flaw.id,
                RelationKind::Enables,
                &text("feeds it"),
            )
            .unwrap();
        assert_eq!(
            store.flaw(flaw.id).unwrap().record.flaw.revision,
            flaw.revision
        );
        assert!(store.candidate(id).unwrap().exposures[0].current);
        assert_eq!(status(&store, id), Status::Shortlisted);
        assert_eq!(store.path_work(path).unwrap().decisions, [other]);
        store
            .add_members(
                &actor,
                id,
                &[Member {
                    flaw: FlawChoice::Existing(other),
                    step: None,
                }],
            )
            .unwrap();
        assess(&store, &actor, id);
        assert_eq!(status(&store, id), Status::Shortlisted);
        store
            .relate_flaws(
                &actor,
                other,
                flaw.id,
                RelationKind::SameRootCause,
                &text("one cause"),
            )
            .unwrap();
        assert_eq!(status(&store, id), Status::Identified);
    }

    #[test]
    fn a_refutation_lifts_only_on_evidence_against_it() {
        let (_directory, store, actor) = fixture();
        let (_, flaw, path) = candidate(&store, &actor, "tail");
        let refutation = store
            .refute_flaw(&actor, flaw, &text("the caller bounds the tail"))
            .unwrap();
        assert!(store.path_work(path).unwrap().decisions.is_empty());

        // Evidence against the flaw agrees with the refutation and leaves it standing.
        store
            .add_evidence(
                &actor,
                EvidenceSubject::Flaw(flaw),
                &contradiction("bounded by the caller"),
            )
            .unwrap();
        assert!(store.flaw(flaw).unwrap().record.flaw.refuted);

        store
            .add_evidence(
                &actor,
                EvidenceSubject::Refutation(refutation),
                &contradiction("a resumed call skips the bound"),
            )
            .unwrap();
        let lifted = store.flaw(flaw).unwrap();
        assert!(!lifted.record.flaw.refuted);
        assert!(lifted.refutation.unwrap().lifted);
        assert_eq!(store.path_work(path).unwrap().decisions, [flaw]);
    }

    #[test]
    fn flaw_merges_carry_refutations_and_decided_pairs() {
        let (_directory, store, actor) = fixture();
        let decoder = component(&store, &actor, "decoder");
        let entry = path(&store, &actor, "entry", &[decoder]);
        let refuted = store
            .add_flaw(&actor, &new_flaw(decoder, "refuted"))
            .unwrap();
        let lifted = store
            .add_flaw(&actor, &new_flaw(decoder, "lifted"))
            .unwrap();
        store
            .refute_flaw(&actor, refuted, &text("bounded upstream"))
            .unwrap();
        let refutation = store
            .refute_flaw(&actor, lifted, &text("bounded upstream"))
            .unwrap();
        store
            .add_evidence(
                &actor,
                EvidenceSubject::Refutation(refutation),
                &contradiction("the bound is skipped on retry"),
            )
            .unwrap();
        let unreached = component(&store, &actor, "huffman");
        let survivor = store
            .add_flaw(&actor, &new_flaw(unreached, "survivor"))
            .unwrap();
        store
            .merge_flaws(&actor, lifted, survivor, &text("same write"))
            .unwrap();
        assert!(!store.flaw(survivor).unwrap().record.flaw.refuted);
        store
            .merge_flaws(&actor, refuted, survivor, &text("same write"))
            .unwrap();
        assert!(store.flaw(survivor).unwrap().record.flaw.refuted);

        // An unplaced decision moved to a component its path never reached stays owed.
        let placed = store
            .add_flaw(&actor, &new_flaw(decoder, "placed"))
            .unwrap();
        contributes(&store, &actor, placed, entry);
        let elsewhere = store
            .add_flaw(&actor, &new_flaw(unreached, "elsewhere"))
            .unwrap();
        store
            .merge_flaws(&actor, placed, elsewhere, &text("same write"))
            .unwrap();
        assert_eq!(store.path_work(entry).unwrap().decisions, [elsewhere]);
    }
}

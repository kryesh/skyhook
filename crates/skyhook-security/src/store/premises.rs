//! Premises: conditions not yet established, each owned by the deployment in scope, a flaw or a
//! candidate. A candidate relies on its own conditions, on the deployment and flaw conditions
//! linked to it, and on its member flaws' conditions, which gate its scenario unless a link says
//! otherwise (`candidate_condition`). An unresolved condition keeps an assessment conditional
//! instead of lowering it, until a verdict scores a metric whose condition does not hold at its
//! least severe value.

use chrono::{DateTime, Utc};
use libsql::Row;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook::db::{Db, enum_column, params};

use super::*;
use crate::cvss::BaseMetric;

/// What a condition gates for a candidate: whether the scenario happens at all, or only how one
/// CVSS base metric scores. A positive verdict needs every scenario condition to hold; while a
/// metric condition does not, the assessment scores that metric at its least severe value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Gate {
    Scenario,
    Metric { metric: BaseMetric },
}

impl Gate {
    fn kind(self) -> GateKind {
        match self {
            Self::Scenario => GateKind::Scenario,
            Self::Metric { .. } => GateKind::Metric,
        }
    }

    fn metric(self) -> Option<BaseMetric> {
        match self {
            Self::Scenario => None,
            Self::Metric { metric } => Some(metric),
        }
    }

    /// A gate from its gate and metric columns at `offset`.
    fn of_columns(row: &Row, offset: i32) -> Result<Self> {
        Ok(match enum_column(row, offset)? {
            GateKind::Scenario => Self::Scenario,
            GateKind::Metric => Self::Metric {
                metric: enum_column(row, offset + 1)?,
            },
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PremiseOn {
    Deployment { deployment: DeploymentId },
    Flaw { id: FlawId },
    Candidate { id: CandidateId, gates: Gate },
}

impl PremiseOn {
    /// The record an event about a condition of this owner concerns.
    pub(super) fn subject(&self) -> Option<Subject> {
        match *self {
            Self::Deployment { .. } => None,
            Self::Flaw { id } => Some(Subject::Flaw(id)),
            Self::Candidate { id, .. } => Some(Subject::Candidate(id)),
        }
    }
}

/// A premise to record: its kind and the condition itself.
#[derive(Clone, Debug)]
pub struct NewPremise {
    pub kind: InvariantKind,
    pub statement: Text,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
pub enum NewCondition {
    Relies {
        gates: Gate,
        premise: PremiseId,
    },
    Own {
        gates: Gate,
        kind: InvariantKind,
        statement: Text,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Resolution {
    /// Citing the current scope's deployment fact admitting it, if any.
    Holds {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deployment: Option<DeploymentId>,
    },
    /// Naming the flaw breaking it, if any.
    Fails {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        flaw: Option<FlawId>,
    },
    /// The scope's facts do not address it: the note is the question for the user.
    Asked,
}

impl Resolution {
    pub fn kind(self) -> ResolutionKind {
        match self {
            Self::Holds { .. } => ResolutionKind::Holds,
            Self::Fails { .. } => ResolutionKind::Fails,
            Self::Asked => ResolutionKind::Asked,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PremiseResolution {
    pub id: ResolutionId,
    /// Made at the premise's current revision.
    pub current: bool,
    pub resolution: Resolution,
    pub note: String,
    pub actor: AgentId,
    pub at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Premise {
    pub id: PremiseId,
    pub on: PremiseOn,
    pub kind: InvariantKind,
    pub statement: String,
    pub revision: u32,
    /// Its latest resolution; without a current one the condition is open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<PremiseResolution>,
    /// Located evidence on the premise, including what reopened it.
    pub evidence: Vec<Evidence>,
    /// The live flaws recorded as breaking it.
    pub undermined_by: Vec<FlawId>,
}

impl Premise {
    /// The current resolution, if any.
    pub fn standing(&self) -> Option<ResolutionKind> {
        (self.resolution.as_ref())
            .filter(|resolution| resolution.current)
            .map(|resolution| resolution.resolution.kind())
    }
}

/// A condition a candidate relies on, with what it gates for that candidate.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Condition {
    pub gates: Gate,
    #[serde(flatten)]
    pub premise: Premise,
}

/// Whether premise `p` is open: a condition a live candidate relies on, without a resolution
/// at the premise's revision.
macro_rules! open_premise {
    () => {
        concat!(
            "(NOT EXISTS (SELECT 1 FROM premise_decision WHERE premise=p.id AND current) AND \
             EXISTS (SELECT 1 FROM candidate_condition oc WHERE oc.premise=p.id AND ",
            paths::live_candidate!("oc.candidate"),
            "))"
        )
    };
}
pub(super) use open_premise;

/// Premises `p` whose standing resolution asks the user under scope `$scope`, joined with
/// that resolution as `x`, until a later scope answers the unanswered fact a condition is on:
/// states it again, of the same kind, admitted or excluded.
macro_rules! asked {
    ($scope:literal) => {
        concat!(
            "premise p JOIN premise_decision x ON x.premise=p.id AND x.current AND \
             x.resolution='asked' AND x.scope=",
            $scope,
            " AND NOT EXISTS (SELECT 1 FROM deployment d JOIN deployment a ON a.scope>d.scope AND \
             a.kind=d.kind AND a.statement=d.statement AND a.stance!='unanswered' WHERE \
             d.id=p.deployment)"
        )
    };
}
pub(super) use asked;

/// Candidate `c` waits on the user's answer: a condition it relies on is asked under the
/// current scope.
macro_rules! awaits_answer {
    () => {
        concat!(
            "EXISTS (SELECT 1 FROM ",
            premises::asked!("(SELECT max(id) FROM scope)"),
            " JOIN candidate_condition ac ON ac.premise=p.id WHERE ac.candidate=c.id)"
        )
    };
}
pub(super) use awaits_answer;

/// Who a premise belongs to, from its deployment kind, flaw, candidate, and its owner's gate
/// and metric columns at `offset`.
pub(super) fn on_columns(row: &Row, offset: i32) -> Result<PremiseOn> {
    Ok(match row.get::<Option<i64>>(offset)? {
        Some(deployment) => PremiseOn::Deployment {
            deployment: DeploymentId(deployment),
        },
        None => match row.get::<Option<i64>>(offset + 1)? {
            Some(flaw) => PremiseOn::Flaw { id: FlawId(flaw) },
            None => PremiseOn::Candidate {
                id: CandidateId(row.get(offset + 2)?),
                gates: Gate::of_columns(row, offset + 3)?,
            },
        },
    })
}

/// A premise's owner columns as [`on_columns`] decodes them, of premise `p` beside the link
/// `o` of the candidate owning it.
macro_rules! owner_columns {
    () => {
        "p.deployment,p.flaw,p.candidate,o.gate,o.metric"
    };
}
pub(super) use owner_columns;

/// The join of [`owner_columns`]' owning candidate's link `o` to premise `p`.
macro_rules! owner_link {
    () => {
        "LEFT JOIN candidate_premise o ON o.premise=p.id AND o.candidate=p.candidate"
    };
}
pub(super) use owner_link;

pub(super) fn premise(db: &Db, id: PremiseId) -> Result<Premise> {
    let premise = one(
        db,
        concat!(
            "SELECT p.id,",
            owner_columns!(),
            ",p.statement,p.revision,p.kind FROM premise p ",
            owner_link!(),
            " WHERE p.id=?1"
        ),
        id,
        |row| {
            Ok(Premise {
                id: PremiseId(row.get(0)?),
                on: on_columns(row, 1)?,
                statement: row.get(6)?,
                revision: row.get::<i64>(7)? as u32,
                kind: enum_column(row, 8)?,
                resolution: None,
                evidence: flaws::evidence_of(db, EvidenceSubject::Premise(id))?,
                undermined_by: consequences::underminers(db, Invariant::Premise(id))?,
            })
        },
    )?;
    let resolution = optional(
        db,
        concat!(
            "SELECT r.id,r.current,r.resolution,r.flaw,r.note,r.at,r.deployment,",
            actor_columns!(),
            " FROM premise_decision r JOIN actor a ON a.id=r.actor WHERE r.premise=?1"
        ),
        params![id.0],
        |row| {
            Ok(PremiseResolution {
                id: ResolutionId(row.get(0)?),
                current: row.get::<i64>(1)? != 0,
                resolution: match enum_column(row, 2)? {
                    ResolutionKind::Holds => Resolution::Holds {
                        deployment: row.get::<Option<i64>>(6)?.map(DeploymentId),
                    },
                    ResolutionKind::Fails => Resolution::Fails {
                        flaw: row.get::<Option<i64>>(3)?.map(FlawId),
                    },
                    ResolutionKind::Asked => Resolution::Asked,
                },
                note: row.get(4)?,
                at: time(row.get(5)?)?,
                actor: actor_column(row, 7)?,
            })
        },
    )?;
    Ok(Premise {
        resolution,
        ..premise
    })
}

/// The conditions candidate `id` relies on, each with what it gates for it.
pub(super) fn of_candidate(db: &Db, id: CandidateId) -> Result<Vec<Condition>> {
    rows(
        db,
        "SELECT premise,gate,metric FROM candidate_condition WHERE candidate=?1 ORDER BY premise",
        params![id.0],
        |row| Ok((PremiseId(row.get(0)?), Gate::of_columns(row, 1)?)),
    )?
    .into_iter()
    .map(|(premise, gates)| {
        Ok(Condition {
            gates,
            premise: self::premise(db, premise)?,
        })
    })
    .collect()
}

/// Who premise `id` belongs to.
pub(super) fn subject(db: &Db, id: PremiseId) -> Result<PremiseOn> {
    Ok(premise(db, id)?.on)
}

/// Whether candidate `candidate` relies on premise `premise`.
pub(super) fn relies(db: &Db, candidate: CandidateId, premise: PremiseId) -> Result<bool> {
    exists(
        db,
        "SELECT 1 FROM candidate_condition WHERE candidate=?1 AND premise=?2",
        params![candidate.0, premise.0],
    )
}

/// The candidates relying on premise `id`; with `scenario`, only those whose scenario it gates.
fn relying(db: &Db, id: PremiseId, scenario: bool) -> Result<Vec<CandidateId>> {
    rows(
        db,
        "SELECT candidate FROM candidate_condition WHERE premise=?1 AND (NOT ?2 OR \
         gate='scenario') ORDER BY candidate",
        params![id.0, scenario],
        |row| Ok(CandidateId(row.get(0)?)),
    )
}

/// Contradicting evidence on a premise: its resolution is owed again. A condition gates its
/// candidates' verdicts rather than their assessments, which were already conditional on it,
/// so their revisions stand.
pub(super) fn touch(db: &Db, actor: &AgentId, id: PremiseId) -> Result<()> {
    db.execute(
        "UPDATE premise SET revision=revision+1 WHERE id=?1",
        params![id.0],
    )?;
    for candidate in relying(db, id, false)? {
        settle_owed(db, actor, candidate, Some(id))?;
    }
    Ok(())
}

/// Plan a follow-up settling each open condition gating the scenario of candidate `id` while
/// its standing verdict is unproven, once per revision of the condition whichever candidate
/// it is planned for; `only` narrows it to the condition just added, linked or reopened.
pub(super) fn settle_owed(
    db: &Db,
    actor: &AgentId,
    id: CandidateId,
    only: Option<PremiseId>,
) -> Result<()> {
    let waiting = rows(
        db,
        "SELECT p.id FROM candidate_condition cc JOIN premise p ON p.id=cc.premise JOIN \
         candidate_state st ON st.candidate=cc.candidate WHERE cc.candidate=?1 AND (?2 IS NULL \
         OR p.id=?2) AND st.state='unproven' AND cc.gate='scenario' AND NOT EXISTS (SELECT 1 \
         FROM premise_decision WHERE premise=p.id AND current) AND NOT EXISTS (SELECT 1 FROM \
         unit_invariant ui JOIN plan_unit u ON u.id=ui.unit WHERE ui.premise=p.id AND \
         ui.revision=p.revision AND u.kind='follow_up' AND u.flaw IS NULL) ORDER BY p.id",
        params![id.0, only.map(|premise| premise.0)],
        |row| Ok(PremiseId(row.get(0)?)),
    )?;
    for premise in waiting {
        plan::owed(
            db,
            actor,
            &UnitWork::FollowUp {
                question: format!(
                    "Candidate #{id}'s review is unproven until Premise #{premise}, a condition gating its scenario, is settled: does it hold?"
                )
                .parse()?,
                stopping: "A resolution of the premise, holding or failing, with its evidence; or the missing fact recorded, and the unit ended inconclusive."
                    .parse()?,
                candidate: Some(id),
                flaw: None,
                invariants: vec![Invariant::Premise(premise)],
            },
        )?;
    }
    Ok(())
}

/// A contradiction of every scenario premise `id` gates: those candidates' tests no longer
/// stand.
pub(super) fn contradict(db: &Db, id: PremiseId) -> Result<()> {
    flaws::contradict(db, &relying(db, id, true)?)
}

/// Insert a condition, which the verdicts of the candidates relying on it wait on; one gating
/// the scenario of an unproven candidate is planned a follow-up settling it.
pub(super) fn insert(
    db: &Db,
    actor: &AgentId,
    on: PremiseOn,
    premise: &NewPremise,
) -> Result<PremiseId> {
    let (deployment, flaw, candidate) = match &on {
        PremiseOn::Deployment { deployment } => {
            scope::cite(db, *deployment, DeploymentStance::Unanswered)?;
            (Some(deployment.0), None, None)
        }
        PremiseOn::Flaw { id } => {
            flaws::live(db, *id)?;
            (None, Some(id.0), None)
        }
        PremiseOn::Candidate { id, .. } => {
            candidates::live(db, *id)?;
            (None, None, Some(id.0))
        }
    };
    let id = PremiseId(db.insert(
        "INSERT INTO premise(deployment,flaw,candidate,kind,statement) VALUES \
         (?1,?2,?3,?4,?5)",
        params![
            deployment,
            flaw,
            candidate,
            premise.kind,
            &premise.statement
        ],
    )?);
    if let PremiseOn::Candidate {
        id: candidate,
        gates,
    } = on
    {
        db.execute(
            "INSERT INTO candidate_premise(candidate,premise,gate,metric) VALUES (?1,?2,?3,?4)",
            params![candidate.0, id.0, gates.kind(), gates.metric()],
        )?;
    }
    let subjects: Vec<Subject> = on.subject().into_iter().collect();
    audit(
        db,
        actor,
        EventKind::Recorded,
        Detail::Record(EventRecord::Premise(id)),
        &subjects,
    )?;
    for candidate in relying(db, id, false)? {
        settle_owed(db, actor, candidate, Some(id))?;
    }
    Ok(id)
}

/// Candidate `candidate` relies on deployment or flaw condition `premise`, gating `gates`;
/// restating a link replaces its gate.
pub(super) fn link(
    db: &Db,
    actor: &AgentId,
    candidate: CandidateId,
    premise: PremiseId,
    gates: Gate,
) -> Result<()> {
    candidates::live(db, candidate)?;
    if let PremiseOn::Candidate { id: owner, .. } = subject(db, premise)? {
        return Err(Error::ScenarioCondition { premise, owner });
    }
    db.execute(
        "INSERT INTO candidate_premise(candidate,premise,gate,metric) VALUES (?1,?2,?3,?4) ON \
         CONFLICT DO UPDATE SET gate=excluded.gate,metric=excluded.metric",
        params![candidate.0, premise.0, gates.kind(), gates.metric()],
    )?;
    audit(
        db,
        actor,
        EventKind::Updated,
        Detail::Record(EventRecord::Premise(premise)),
        &[Subject::Candidate(candidate)],
    )?;
    settle_owed(db, actor, candidate, Some(premise))
}

impl Store {
    pub fn add_premise(
        &self,
        actor: &AgentId,
        on: PremiseOn,
        premise: &NewPremise,
    ) -> Result<PremiseId> {
        self.write(|db| insert(db, actor, on, premise))
    }

    /// The candidate relies on a deployment or flaw condition ([`link`]).
    pub fn rely(
        &self,
        actor: &AgentId,
        candidate: CandidateId,
        premise: PremiseId,
        gates: Gate,
    ) -> Result<()> {
        self.write(|db| link(db, actor, candidate, premise, gates))
    }

    /// Settles a condition at its current revision; the latest resolution stands. A positive
    /// verdict stands only while every condition gating its scenario holds, and one found
    /// failing contradicts every scenario it gates, so their tests no longer stand. A holding
    /// condition may cite the deployment fact the current scope admits that it holds by. One
    /// the scope's facts do not address is asked of the user under the current scope, which
    /// keeps the review of every candidate relying on it back until a later scope answers it.
    /// A flaw's condition is a fact about the code: it is never asked, and no deployment fact
    /// decides it. Restating a standing resolution, asked under the same scope, records
    /// nothing.
    pub fn resolve_premise(
        &self,
        actor: &AgentId,
        premise: PremiseId,
        resolution: Resolution,
        note: &Text,
    ) -> Result<ResolutionId> {
        self.write(|db| {
            let on = subject(db, premise)?;
            if matches!(on, PremiseOn::Flaw { .. })
                && matches!(
                    resolution,
                    Resolution::Asked
                        | Resolution::Holds {
                            deployment: Some(_)
                        }
                )
            {
                return Err(Error::CodeFact { premise });
            }
            let (flaw, deployment, scope) = match resolution {
                Resolution::Holds { deployment } => {
                    if let Some(deployment) = deployment {
                        scope::cite(db, deployment, DeploymentStance::Admitted)?;
                    }
                    (None, deployment, None)
                }
                Resolution::Fails { flaw } => {
                    if let Some(flaw) = flaw {
                        flaws::live(db, flaw)?;
                    }
                    (flaw, None, None)
                }
                Resolution::Asked => {
                    let scope = scope::policy(db)?.ok_or_else(|| Record::Scope.missing())?;
                    (None, None, Some(scope))
                }
            };
            let (flaw, deployment, scope) = (
                flaw.map(|id| id.0),
                deployment.map(|id| id.0),
                scope.map(|id| id.0),
            );
            if let Some(standing) = optional(
                db,
                "SELECT id FROM premise_decision WHERE premise=?1 AND current AND resolution=?2 \
                 AND flaw IS ?3 AND deployment IS ?4 AND scope IS ?5",
                params![premise.0, resolution.kind(), flaw, deployment, scope],
                |row| Ok(ResolutionId(row.get(0)?)),
            )? {
                return Ok(standing);
            }
            let subjects: Vec<Subject> = (relying(db, premise, false)?.into_iter())
                .map(Subject::Candidate)
                .chain(flaw.map(|id| Subject::Flaw(FlawId(id))))
                .collect();
            let actor_id = actor_row(db, actor)?;
            let id = ResolutionId(db.insert(
                "INSERT INTO premise_resolution(premise,revision,resolution,flaw,deployment,scope,note,actor,at) SELECT id,revision,?2,?3,?4,?5,?6,?7,?8 FROM premise WHERE id=?1",
                params![premise.0, resolution.kind(), flaw, deployment, scope, note, actor_id, now()],
            )?);
            if let Resolution::Fails { .. } = resolution {
                contradict(db, premise)?;
            }
            audit(db, actor, EventKind::Resolved, Detail::Record(EventRecord::Resolution(id)), &subjects)?;
            Ok(id)
        })
    }

    pub fn premise(&self, id: PremiseId) -> Result<Premise> {
        self.read(|db| premise(db, id))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::tests::{
            answerer, assess, candidate, contradiction, fixture, holds, new_candidate, new_premise,
            queued, scenario, status, text, verdict,
        },
        *,
    };

    /// A flaw's condition gates the scenario of every candidate built on the flaw, and a
    /// deployment condition that of every candidate relying on it: asked once, it holds back
    /// each of their reviews until a later scope answers its fact, and failing, it contradicts
    /// each scenario it gates. A
    /// re-assessment relying on a condition the judged one did not keeps no verdict. A flaw's
    /// condition is settled from the code and follows the flaw into a merge.
    #[test]
    fn a_shared_condition_gates_every_candidate_relying_on_it() {
        let (_directory, store, actor) = fixture();
        let (first, flaw, path) = candidate(&store, &actor, "decoder");
        let sibling = new_candidate(FlawChoice::Existing(flaw), vec![path]);
        let second = store.add_candidate(&actor, &sibling).unwrap();
        let both = [first, second];
        for id in both {
            assess(&store, &actor, id);
        }
        let reviewer = answerer(&actor);
        verdict(&store, &reviewer, first, Verdict::Validated);
        let header = new_premise(InvariantKind::State, "the block header was read");
        let read = (store.add_premise(&actor, PremiseOn::Flaw { id: flaw }, &header)).unwrap();
        assert_eq!(
            status(&store, first),
            Status::Shortlisted,
            "it inherits the open condition"
        );
        assert!(matches!(
            store.resolve_premise(&reviewer, read, Resolution::Asked, &text("is it read?")),
            Err(Error::CodeFact { .. })
        ));
        holds(&store, &actor, read);
        assert_eq!(status(&store, first), Status::Validated);
        let guarded = contradiction("the copy is guarded upstream");
        (store.add_evidence(&actor, EvidenceSubject::Candidate(first), &guarded)).unwrap();
        assess(&store, &actor, first);
        assert_eq!(status(&store, first), Status::Shortlisted);

        let option = NewDeployment {
            kind: DeploymentKind::BuildFlag,
            stance: DeploymentStance::Unanswered,
            statement: text("the large-window option is built in"),
        };
        store
            .add_scope(&actor, &text("shipped"), std::slice::from_ref(&option))
            .unwrap();
        let scope = || {
            (Listing::drain(|page| store.list_scopes(page))
                .unwrap()
                .pop())
            .unwrap()
        };
        let built = PremiseOn::Deployment {
            deployment: scope().deployment[0].id,
        };
        let flag = new_premise(InvariantKind::State, "the option is built in");
        let flag = store.add_premise(&actor, built, &flag).unwrap();
        for id in both {
            store.rely(&actor, id, flag, Gate::Scenario).unwrap();
        }
        let question = text("is the large-window option built?");
        (store.resolve_premise(&reviewer, flag, Resolution::Asked, &question)).unwrap();
        let asked: Vec<_> = scope().asked.iter().map(|asked| asked.candidate).collect();
        assert_eq!(asked, both, "one question for both");
        for id in both {
            verdict(&store, &reviewer, id, Verdict::Unproven);
        }
        assert!(
            queued(&store, Phase::Review).is_empty(),
            "both wait on the answer"
        );
        let admitted = NewDeployment {
            stance: DeploymentStance::Admitted,
            ..option
        };
        (store.add_scope(&actor, &text("shipped"), &[admitted])).unwrap();
        (store.resolve_premise(&reviewer, flag, Resolution::Asked, &question)).unwrap();
        assert!(scope().asked.is_empty(), "the later scope answered it");

        let contradictions = |id: CandidateId| {
            let sql = "SELECT contradictions FROM candidate WHERE id=?1";
            (store.read(|db| stored(db, sql, params![id.0], |row| Ok(row.get::<i64>(0)?)))).unwrap()
        };
        let before = both.map(contradictions);
        let fails = Resolution::Fails { flaw: None };
        (store.resolve_premise(&reviewer, read, fails, &text("a retry skips it"))).unwrap();
        assert_eq!(both.map(contradictions), before.map(|n| n + 1));

        let chunked = new_premise(InvariantKind::State, "the stream is chunked");
        let own = store
            .add_premise(&actor, scenario(second), &chunked)
            .unwrap();
        assert!(matches!(
            store.rely(&actor, first, own, Gate::Scenario),
            Err(Error::ScenarioCondition { .. })
        ));
        let (_, framing, _) = candidate(&store, &actor, "framing");
        let split = new_premise(InvariantKind::State, "a frame boundary splits the length");
        let split = (store.add_premise(&actor, PremiseOn::Flaw { id: framing }, &split)).unwrap();
        (store.merge_flaws(&actor, framing, flaw, &text("one root cause"))).unwrap();
        let conditions = store.candidate(first).unwrap().premises;
        assert!(
            conditions
                .iter()
                .any(|condition| condition.premise.id == split)
        );
    }
}

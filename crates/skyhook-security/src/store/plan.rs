//! The review plan and what became of it: a sweep of each reached component, a review of
//! each path, questions and follow-ups, each ending in a typed outcome with its
//! limitation. The plan is the run's coverage account; nothing reopens a unit on its own.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use libsql::Row;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook::db::{Db, enum_column, params};

use super::*;

/// Any analysis agent may add a follow-up; a question or a follow-up names what would
/// settle it (`stopping`) and the guarantees and premises it asks about, and a
/// follow-up about one candidate names it, so the candidate's tests wait until the follow-up
/// ends. A follow-up tracing a flaw names it, asking whether it breaks the guarantees and
/// premises it names; one without a flaw settles premises of the candidate it names.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UnitWork {
    Sweep {
        component: ComponentId,
    },
    PathReview {
        path: PathId,
    },
    Question {
        question: Text,
        stopping: Text,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        invariants: Vec<Invariant>,
        #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")]
        check: Option<Check>,
    },
    FollowUp {
        question: Text,
        stopping: Text,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        candidate: Option<CandidateId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        flaw: Option<FlawId>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        invariants: Vec<Invariant>,
    },
}

impl UnitWork {
    /// The records a question or follow-up is about: its flaw, its candidate, its guarantees
    /// and premises, and the function a check asks about.
    pub fn about(&self) -> Vec<About> {
        let (flaw, candidate, invariants) = match self {
            Self::Question { invariants, .. } => (None, None, invariants),
            Self::FollowUp {
                flaw,
                candidate,
                invariants,
                ..
            } => (*flaw, *candidate, invariants),
            Self::Sweep { .. } | Self::PathReview { .. } => return Vec::new(),
        };
        (flaw.map(About::Flaw).into_iter())
            .chain(candidate.map(About::Candidate))
            .chain(invariants.iter().map(|invariant| About::from(*invariant)))
            .chain(self.check().map(|check| About::Function {
                component: check.component,
                function: check.function.to_string(),
            }))
            .collect()
    }

    pub fn check(&self) -> Option<&Check> {
        match self {
            Self::Question { check, .. } => check.as_ref(),
            _ => None,
        }
    }
}

/// A premise an inconclusive question or follow-up leaves open, with the sites it rests on
/// that the unit did not trace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OpenPremise {
    pub premise: PremiseId,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub untraced: Vec<Location>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct UnitOutcome {
    pub id: OutcomeId,
    pub outcome: Outcome,
    /// What was done and its limitation.
    pub note: String,
    /// The premises an inconclusive question or follow-up left open.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open: Vec<OpenPremise>,
    pub actor: AgentId,
    pub at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct PlanUnit {
    pub id: UnitId,
    #[serde(flatten)]
    pub work: UnitWork,
    /// Its one outcome; none while the unit is open.
    pub outcome: Option<UnitOutcome>,
    /// The agent that asked for it, or the root agent for a unit the store planned.
    pub planned_by: AgentId,
    pub at: DateTime<Utc>,
    /// For a check read on its own, what bore on its contract since it was planned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<Vec<EventRecord>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanFilter {
    pub kind: Option<UnitKind>,
    /// Units whose standing outcome is this; open units are what the queues list.
    pub outcome: Option<Outcome>,
}

/// One kind's units: those still open and those ended, by outcome.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct Tally {
    pub open: u64,
    pub ended: BTreeMap<Outcome, u64>,
}

impl Tally {
    pub fn ended(&self, outcome: Outcome) -> u64 {
        self.ended.get(&outcome).copied().unwrap_or_default()
    }

    /// Units that ended with a limitation: blocked, inconclusive or deferred.
    pub fn limited(&self) -> u64 {
        (self.ended.iter())
            .filter(|(outcome, _)| **outcome != Outcome::Completed)
            .map(|(_, count)| count)
            .sum()
    }

    pub fn total(&self) -> u64 {
        self.open + self.ended.values().sum::<u64>()
    }
}

/// Where the plan stands once coverage is reconciled with it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PlanStanding {
    #[default]
    NotPlanned,
    /// Some unit is still open.
    InProgress,
    /// Every unit has an outcome. Coverage can still owe review the plan holds no unit for
    /// (a flaw recorded in a component after its paths were reviewed): those paths and
    /// components are named, and the review is complete only when there are none.
    Ended {
        unchecked: Vec<PathId>,
        uninspected: Vec<ComponentId>,
    },
}

/// The plan against its outcomes and coverage, and the finding work still owed.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct Synthesis {
    pub units: BTreeMap<UnitKind, Tally>,
    pub standing: PlanStanding,
    /// What each finding phase still owes.
    pub findings: BTreeMap<Phase, u64>,
}

impl Synthesis {
    pub fn tally(&self, kind: UnitKind) -> Tally {
        self.units.get(&kind).cloned().unwrap_or_default()
    }

    pub fn open(&self) -> u64 {
        self.units.values().map(|tally| tally.open).sum()
    }

    /// Units of every kind that ended with `outcome`.
    pub fn ended(&self, outcome: Outcome) -> u64 {
        self.units.values().map(|tally| tally.ended(outcome)).sum()
    }

    pub fn limited(&self) -> u64 {
        self.units.values().map(Tally::limited).sum()
    }

    pub fn total(&self) -> u64 {
        self.units.values().map(Tally::total).sum()
    }

    pub fn owed(&self, phase: Phase) -> u64 {
        self.findings.get(&phase).copied().unwrap_or_default()
    }

    /// Whether the planned review is complete: every unit ended and coverage owes nothing.
    pub fn complete(&self) -> bool {
        matches!(&self.standing, PlanStanding::Ended { unchecked, uninspected } if unchecked.is_empty() && uninspected.is_empty())
    }

    /// The plan in one line: how many units it holds, how many are still open or what
    /// coverage still owes, and those that ended with a limitation.
    pub fn line(&self) -> String {
        let total = self.total();
        let mut line = match &self.standing {
            PlanStanding::NotPlanned => return "no review planned yet".to_owned(),
            PlanStanding::InProgress => format!(
                "planned review in progress: {} of {total} units open",
                self.open()
            ),
            PlanStanding::Ended {
                unchecked,
                uninspected,
            } if unchecked.is_empty() && uninspected.is_empty() => {
                format!("planned review complete: {total} units")
            }
            PlanStanding::Ended {
                unchecked,
                uninspected,
            } => {
                let listed = |ids: Vec<String>| ids.join(", ");
                let owed: Vec<String> = [
                    (!unchecked.is_empty()).then(|| {
                        format!(
                            "paths {} unchecked",
                            listed(unchecked.iter().map(ToString::to_string).collect())
                        )
                    }),
                    (!uninspected.is_empty()).then(|| {
                        format!(
                            "reached components {} uninspected",
                            listed(uninspected.iter().map(ToString::to_string).collect())
                        )
                    }),
                ]
                .into_iter()
                .flatten()
                .collect();
                format!(
                    "planned units ended: {total} units, but coverage still owes review: {}",
                    owed.join("; ")
                )
            }
        };
        let limited: Vec<String> = [Outcome::Blocked, Outcome::Inconclusive, Outcome::Deferred]
            .into_iter()
            .filter_map(|outcome| {
                let count = self.ended(outcome);
                (count > 0).then(|| format!("{count} {outcome}"))
            })
            .collect();
        if !limited.is_empty() {
            line.push_str("; ");
            line.push_str(&limited.join(", "));
        }
        line
    }
}

/// How much of the run's code and paths the plan has examined.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct Coverage {
    pub components: BTreeMap<ComponentCoverage, u64>,
    pub paths: BTreeMap<PathCoverage, u64>,
    /// Deferred regions no later examination settled.
    pub deferred: u64,
    /// Live candidates' conditions without a resolution.
    pub open_premises: u64,
    /// Contracts by their latest check.
    pub contracts: BTreeMap<ContractState, u64>,
    /// Load-bearing guarantees untrusted input reaches that no supported check names and no
    /// question or follow-up asks about, other than one ended deferred.
    pub unchallenged: u64,
}

impl Coverage {
    pub fn components_in(&self, state: ComponentCoverage) -> u64 {
        self.components.get(&state).copied().unwrap_or_default()
    }

    pub fn paths_in(&self, state: PathCoverage) -> u64 {
        self.paths.get(&state).copied().unwrap_or_default()
    }

    pub fn contracts_in(&self, state: ContractState) -> u64 {
        self.contracts.get(&state).copied().unwrap_or_default()
    }
}

macro_rules! unit_select {
    () => {
        concat!(
            "SELECT u.id,u.kind,u.component,u.path,u.question,u.stopping,u.candidate,u.at,u.flaw,",
            actor_columns!(),
            ",k.contract,c.component,c.function,c.kind,k.producer,k.input FROM plan_unit u JOIN \
             actor a ON a.id=u.actor LEFT JOIN unit_check k ON k.unit=u.id LEFT JOIN contract c ON \
             c.id=k.contract WHERE "
        )
    };
}

/// The outcome of unit `u`, NULL while it is open.
macro_rules! unit_outcome {
    () => {
        "(SELECT kind FROM outcome WHERE unit=u.id)"
    };
}
pub(super) use unit_outcome;

/// No unit asks about the guarantee or premise whose `$column` is `$id`, other than one
/// ended deferred: an open unit, or one that ended otherwise, is an attempt on it.
macro_rules! unchallenged {
    ($column:literal, $id:literal) => {
        concat!(
            "NOT EXISTS (SELECT 1 FROM unit_invariant uc JOIN plan_unit u ON u.id=uc.unit WHERE uc.",
            $column,
            "=",
            $id,
            " AND ",
            plan::unit_outcome!(),
            " IS NOT 'deferred')"
        )
    };
}

fn unit_row(row: &Row) -> Result<PlanUnit> {
    let kind: UnitKind = enum_column(row, 1)?;
    let text = |index| -> Result<Text> { Ok(row.get::<String>(index)?.parse()?) };
    let work = match kind {
        UnitKind::Sweep => UnitWork::Sweep {
            component: ComponentId(row.get(2)?),
        },
        UnitKind::PathReview => UnitWork::PathReview {
            path: PathId(row.get(3)?),
        },
        UnitKind::Question => UnitWork::Question {
            question: text(4)?,
            stopping: text(5)?,
            invariants: Vec::new(),
            check: match row.get::<Option<i64>>(10)? {
                Some(contract) => Some(Check {
                    contract: ContractId(contract),
                    component: ComponentId(row.get(11)?),
                    function: row.get(12)?,
                    kind: enum_column(row, 13)?,
                    producer: row.get(14)?,
                    input: row.get(15)?,
                }),
                None => None,
            },
        },
        UnitKind::FollowUp => UnitWork::FollowUp {
            question: text(4)?,
            stopping: text(5)?,
            candidate: row.get::<Option<i64>>(6)?.map(CandidateId),
            flaw: row.get::<Option<i64>>(8)?.map(FlawId),
            invariants: Vec::new(),
        },
    };
    Ok(PlanUnit {
        id: UnitId(row.get(0)?),
        work,
        outcome: None,
        at: time(row.get(7)?)?,
        planned_by: actor_column(row, 9)?,
        since: None,
    })
}

fn hydrate(db: &Db, mut unit: PlanUnit) -> Result<PlanUnit> {
    if let UnitWork::Question { invariants, .. } | UnitWork::FollowUp { invariants, .. } =
        &mut unit.work
    {
        *invariants = rows(
            db,
            "SELECT guarantee,premise FROM unit_invariant WHERE unit=?1 ORDER BY rowid",
            params![unit.id.0],
            |row| {
                Ok(Invariant::of_columns(row.get(0)?, row.get(1)?)
                    .expect("a guarantee or a premise is set"))
            },
        )?;
    }
    unit.outcome = optional(
        db,
        concat!(
            "SELECT o.id,o.kind,o.note,o.at,",
            actor_columns!(),
            " FROM outcome o JOIN actor a ON a.id=o.actor WHERE o.unit=?1"
        ),
        params![unit.id.0],
        |row| {
            Ok(UnitOutcome {
                id: OutcomeId(row.get(0)?),
                outcome: enum_column(row, 1)?,
                note: row.get(2)?,
                open: Vec::new(),
                at: time(row.get(3)?)?,
                actor: actor_column(row, 4)?,
            })
        },
    )?;
    if let Some(outcome) = &mut unit.outcome {
        outcome.open = open_of(db, outcome.id)?;
    }
    Ok(unit)
}

/// The premises outcome `id` left open, each with its untraced sites.
fn open_of(db: &Db, id: OutcomeId) -> Result<Vec<OpenPremise>> {
    let mut open: Vec<OpenPremise> = Vec::new();
    for (premise, untraced) in rows(
        db,
        concat!(
            "SELECT op.premise,",
            scope::location_columns!("l"),
            " FROM outcome_premise op LEFT JOIN outcome_untraced ut ON ut.outcome=op.outcome AND \
             ut.premise=op.premise LEFT JOIN location l ON l.id=ut.location WHERE op.outcome=?1 \
             ORDER BY op.premise,l.file,l.start_line,l.end_line"
        ),
        params![id.0],
        |row| {
            let untraced = match row.get::<Option<String>>(1)? {
                Some(_) => Some(scope::location_column(row, 1)?),
                None => None,
            };
            Ok((PremiseId(row.get(0)?), untraced))
        },
    )? {
        match open.last_mut() {
            Some(last) if last.premise == premise => last.untraced.extend(untraced),
            _ => open.push(OpenPremise {
                premise,
                untraced: untraced.into_iter().collect(),
            }),
        }
    }
    Ok(open)
}

/// Refuse what an outcome leaves open unless it fits: only an inconclusive outcome names open
/// premises, an inconclusive question or follow-up other than a check names at least one, and
/// a flaw's condition it names, a fact about the code, lists the sites enumerated as evidence
/// on it and some site not traced.
fn check_open(db: &Db, work: &UnitWork, outcome: Outcome, open: &[OpenPremise]) -> Result<()> {
    if open.len() > MAX_ATTACHMENTS {
        return Err(Error::Limit(Bound::Attachments));
    }
    match (outcome, work) {
        (
            Outcome::Inconclusive,
            UnitWork::Question { check: None, .. } | UnitWork::FollowUp { .. },
        ) if open.is_empty() => return Err(Error::InconclusiveNeedsPremise),
        (Outcome::Inconclusive, _) => {}
        _ if open.is_empty() => {}
        _ => return Err(Error::OpenWithoutInconclusive),
    }
    for entry in open {
        if let PremiseOn::Flaw { .. } = premises::subject(db, entry.premise)?
            && (entry.untraced.is_empty()
                || !exists(
                    db,
                    "SELECT 1 FROM evidence WHERE premise=?1",
                    params![entry.premise.0],
                )?)
        {
            return Err(Error::UntracedSites {
                premise: entry.premise,
            });
        }
    }
    Ok(())
}

pub(super) fn unit(db: &Db, id: UnitId) -> Result<PlanUnit> {
    let unit = one(db, concat!(unit_select!(), "u.id=?1"), id, unit_row)?;
    hydrate(db, unit)
}

/// The sweep units of `component`, oldest first.
pub(super) fn sweeps_of(db: &Db, component: ComponentId) -> Result<Vec<PlanUnit>> {
    rows(
        db,
        concat!(unit_select!(), "u.component=?1 ORDER BY u.id"),
        params![component.0],
        unit_row,
    )?
    .into_iter()
    .map(|unit| hydrate(db, unit))
    .collect()
}

/// Insert a unit `actor` plans. A follow-up asking about a guarantee names the flaw it asks
/// about; without a flaw it names only conditions of its candidate.
pub(super) fn insert(db: &Db, actor: &AgentId, input: &UnitWork) -> Result<UnitId> {
    let (kind, component, path, question, stopping, candidate) = match input {
        UnitWork::Sweep { component } => {
            scope::known(db, *component)?;
            (UnitKind::Sweep, Some(component.0), None, None, None, None)
        }
        UnitWork::PathReview { path } => {
            paths::revision(db, *path)?;
            (UnitKind::PathReview, None, Some(path.0), None, None, None)
        }
        UnitWork::Question {
            question, stopping, ..
        } => (
            UnitKind::Question,
            None,
            None,
            Some(question),
            Some(stopping),
            None,
        ),
        UnitWork::FollowUp {
            question,
            stopping,
            candidate,
            ..
        } => (
            UnitKind::FollowUp,
            None,
            None,
            Some(question),
            Some(stopping),
            *candidate,
        ),
    };
    let (flaw, invariants) = match input {
        UnitWork::FollowUp {
            flaw, invariants, ..
        } => (*flaw, invariants.as_slice()),
        UnitWork::Question { invariants, .. } => (None, invariants.as_slice()),
        _ => (None, [].as_slice()),
    };
    if invariants.len() > MAX_ATTACHMENTS {
        return Err(Error::Limit(Bound::Attachments));
    }
    if let Some(flaw) = flaw {
        flaws::live(db, flaw)?;
    }
    let settles = kind == UnitKind::FollowUp && flaw.is_none();
    for invariant in invariants {
        match (settles, invariant) {
            (false, invariant) => invariant.known(db)?,
            (true, Invariant::Premise(premise)) => {
                premises::subject(db, *premise)?;
                let relied = match candidate {
                    Some(candidate) => premises::relies(db, candidate, *premise)?,
                    None => false,
                };
                if !relied {
                    return Err(Error::InvariantWithoutFlaw);
                }
            }
            (true, Invariant::Guarantee(_)) => return Err(Error::InvariantWithoutFlaw),
        }
    }
    if let Some(candidate) = candidate {
        candidates::live(db, candidate)?;
    }
    let actor_id = actor_row(db, actor)?;
    let id = UnitId(db.insert(
        "INSERT INTO plan_unit(kind,component,path,question,stopping,candidate,flaw,actor,at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            kind,
            component,
            path,
            question,
            stopping,
            candidate.map(|id| id.0),
            flaw.map(|id| id.0),
            actor_id,
            now()
        ],
    )?);
    if let Some(check) = input.check() {
        db.execute(
            "INSERT INTO unit_check(unit,contract,producer,input) VALUES (?1,?2,?3,?4)",
            params![
                id.0,
                check.contract.0,
                check.producer.as_deref(),
                check.input.as_deref()
            ],
        )?;
    }
    for invariant in invariants {
        db.execute(
            "INSERT INTO unit_invariant(unit,guarantee,premise,revision) VALUES (?1,?2,?3,(SELECT revision FROM premise WHERE id=?3)) ON CONFLICT DO NOTHING",
            params![
                id.0,
                invariant.guarantee().map(|id| id.0),
                invariant.premise().map(|id| id.0)
            ],
        )?;
    }
    let subjects: Vec<Subject> = (candidate.map(Subject::Candidate).into_iter())
        .chain(flaw.map(Subject::Flaw))
        .collect();
    audit(
        db,
        actor,
        EventKind::Recorded,
        Detail::Record(EventRecord::Unit(id)),
        &subjects,
    )?;
    Ok(id)
}

/// Plan a unit the store owes after `actor`'s record. The run plans it, as the root agent,
/// so `actor` may complete it like any other agent.
pub(super) fn owed(db: &Db, actor: &AgentId, input: &UnitWork) -> Result<UnitId> {
    let run = skyhook::identity::AgentId::root(actor.session());
    insert(db, &run, input)
}

/// Plan a sweep of `component` when a path reaches it, its sweeps do not cover it as it
/// is now, and no sweep of it is open. Called where reach or files change, so a change
/// that leaves a component uninspected is planned; a sweep that ended blocked or
/// inconclusive is a recorded limitation and plans nothing on its own.
pub(super) fn sweep_owed(db: &Db, actor: &AgentId, component: ComponentId) -> Result<()> {
    if exists(
        db,
        concat!(
            "SELECT 1 FROM component c WHERE c.id=?1 AND ",
            components::component_coverage!(),
            "='reached' AND NOT ",
            queues::open_sweep!("c.id")
        ),
        params![component.0],
    )? {
        owed(db, actor, &UnitWork::Sweep { component })?;
    }
    Ok(())
}

/// The files a sweep of `component` completed by `actor` covers: those it held when the
/// agent started, or every one it holds for an agent the journal holds no start for.
fn swept_files(db: &Db, actor: &AgentId, component: ComponentId) -> Result<Vec<String>> {
    rows(
        db,
        "SELECT cf.file FROM component_file cf WHERE cf.component=?1 AND \
         cf.joined<coalesce((SELECT j.created FROM actor a JOIN agent g ON g.id=a.session_agent \
         JOIN job j ON j.id=g.owner_job WHERE a.id=?2),cf.joined+1) ORDER BY cf.file",
        params![component.0, actor_row(db, actor)?],
        |row| Ok(row.get(0)?),
    )
}

/// Plan a review of `path` unless one is open: its membership changed, so every decision
/// it made is owed again.
pub(super) fn review_owed(db: &Db, actor: &AgentId, path: PathId) -> Result<()> {
    if !exists(
        db,
        concat!(
            "SELECT 1 FROM plan_unit u WHERE u.path=?1 AND ",
            queues::open_unit!("u")
        ),
        params![path.0],
    )? {
        owed(db, actor, &UnitWork::PathReview { path })?;
    }
    Ok(())
}

/// Record how `unit` ended.
fn end(db: &Db, actor: &AgentId, unit: UnitId, outcome: Outcome, note: &str) -> Result<OutcomeId> {
    let id = OutcomeId(db.insert(
        "INSERT INTO outcome(unit,kind,note,actor,at) VALUES (?1,?2,?3,?4,?5)",
        params![unit.0, outcome, note, actor_row(db, actor)?, now()],
    )?);
    audit(
        db,
        actor,
        EventKind::Recorded,
        Detail::Record(EventRecord::Outcome(id)),
        &[],
    )?;
    Ok(id)
}

/// Move `duplicate`'s units to `into` when every premise they name moved with the merge;
/// the rest stay with the duplicate, and those still open end deferred.
pub(super) fn merge(
    db: &Db,
    actor: &AgentId,
    duplicate: CandidateId,
    into: CandidateId,
) -> Result<()> {
    db.execute(
        "UPDATE plan_unit SET candidate=?2 WHERE candidate=?1 AND NOT EXISTS (SELECT 1 FROM \
         unit_invariant ui JOIN premise p ON p.id=ui.premise WHERE ui.unit=plan_unit.id AND \
         p.candidate=?1)",
        params![duplicate.0, into.0],
    )?;
    let note = format!("merged into #{}", into.0);
    for unit in rows(
        db,
        concat!(
            "SELECT u.id FROM plan_unit u WHERE u.candidate=?1 AND ",
            queues::open_unit!("u"),
            " ORDER BY u.id"
        ),
        params![duplicate.0],
        |row| Ok(UnitId(row.get(0)?)),
    )? {
        end(db, actor, unit, Outcome::Deferred, &note)?;
    }
    Ok(())
}

impl Store {
    /// A unit `actor` asks for. A follow-up about a candidate is refused to the agent that
    /// stands behind a positive verdict on it: a reviewer with such a doubt records the
    /// verdict unproven, with the open question as a premise.
    pub fn plan_unit(&self, actor: &AgentId, input: &UnitWork) -> Result<UnitId> {
        self.write(|db| {
            if let UnitWork::FollowUp {
                candidate: Some(candidate),
                ..
            } = input
                && exists(
                    db,
                    "SELECT 1 FROM candidate_standing st JOIN validation v ON v.id=st.validation \
                     JOIN actor a ON a.id=v.actor WHERE st.candidate=?1 AND \
                     v.verdict='validated' AND a.agent=?2",
                    params![candidate.0, actor.to_string()],
                )?
            {
                return Err(Error::QuestionsOwnVerdict {
                    candidate: *candidate,
                });
            }
            insert(db, actor, input)
        })
    }

    /// How a unit ended, once: a second outcome is refused. Completing a unit answers it, so
    /// the agent that planned it never does, nor, for a check, an agent whose record triggered
    /// it; either may only end it with a limitation. An
    /// inconclusive question or follow-up names the premises it leaves open (`open`). A sweep
    /// covers the files its component held when the agent recording it started, which then
    /// stay with the component; a file that joined later owes another sweep. Recorded by an
    /// agent the journal holds no start for, it covers every file the component holds now. It
    /// completes once every retry site in the files it covers is classified and every
    /// guarantee of its component spanning a retry status cites each site of it there.
    pub fn record_outcome(
        &self,
        actor: &AgentId,
        unit: UnitId,
        outcome: Outcome,
        note: &Text,
        open: &[OpenPremise],
    ) -> Result<OutcomeId> {
        self.write(|db| {
            let recorded = self::unit(db, unit)?;
            if recorded.outcome.is_some() {
                return Err(Error::UnitEnded { unit });
            }
            if outcome == Outcome::Completed
                && (recorded.planned_by == *actor
                    || exists(
                        db,
                        "SELECT 1 FROM unit_check k JOIN contract_trigger t ON \
                         t.contract=k.contract JOIN actor a ON a.id=t.actor WHERE k.unit=?1 AND \
                         t.event<(SELECT event FROM event_record WHERE unit=?1) AND a.agent=?2",
                        params![unit.0, actor.to_string()],
                    )?)
            {
                return Err(Error::OwnUnit { unit });
            }
            check_open(db, &recorded.work, outcome, open)?;
            let swept = match (recorded.work, outcome) {
                (UnitWork::Sweep { component }, Outcome::Completed) => {
                    let files = swept_files(db, actor, component)?;
                    retry::swept(db, component, &files)?;
                    Some((component, files))
                }
                _ => None,
            };
            let id = end(db, actor, unit, outcome, note.as_str())?;
            for entry in open {
                db.execute(
                    "INSERT INTO outcome_premise(outcome,premise) VALUES (?1,?2) ON CONFLICT DO \
                     NOTHING",
                    params![id.0, entry.premise.0],
                )?;
                for location in &entry.untraced {
                    let location = scope::location_id(db, location)?;
                    db.execute(
                        "INSERT INTO outcome_untraced(outcome,premise,location) VALUES (?1,?2,?3) \
                         ON CONFLICT DO NOTHING",
                        params![id.0, entry.premise.0, location.0],
                    )?;
                }
            }
            if let Some((component, files)) = swept {
                db.execute(
                    "INSERT INTO swept_file(outcome,file) SELECT ?1,value FROM json_each(?2)",
                    params![id.0, json_list(&files)?],
                )?;
                sweep_owed(db, actor, component)?;
            }
            Ok(id)
        })
    }

    /// The unit, and for a check what was recorded in its function since it was planned.
    pub fn unit(&self, id: UnitId) -> Result<PlanUnit> {
        self.read(|db| {
            let mut unit = unit(db, id)?;
            if unit.work.check().is_some() {
                unit.since = Some(checks::since(db, id)?);
            }
            Ok(unit)
        })
    }

    pub fn plan(&self, filter: &PlanFilter, page: Page) -> Result<Listing<PlanUnit>> {
        self.read(|db| {
            let units = rows(
                db,
                concat!(
                    unit_select!(),
                    "u.id>?1 AND (?3 IS NULL OR u.kind=?3) AND (?4 IS NULL OR ",
                    unit_outcome!(),
                    "=?4) ORDER BY u.id LIMIT ?2"
                ),
                params![page.after, page.limit(), filter.kind, filter.outcome],
                unit_row,
            )?
            .into_iter()
            .map(|unit| hydrate(db, unit))
            .collect::<Result<_>>()?;
            Ok(Listing::keyset(units, page))
        })
    }

    /// Units that ended with a limitation first, then open ones, then completed ones.
    pub fn plan_rows(&self, limit: usize) -> Result<Vec<PlanUnit>> {
        self.read(|db| {
            rows(
                db,
                concat!(
                    unit_select!(),
                    "1 ORDER BY CASE coalesce(",
                    unit_outcome!(),
                    ",'') WHEN '' THEN 1 WHEN 'completed' THEN 2 ELSE 0 END, u.id LIMIT ?1"
                ),
                params![limit],
                unit_row,
            )?
            .into_iter()
            .map(|unit| hydrate(db, unit))
            .collect()
        })
    }

    /// The plan against its outcomes, with the finding work still owed.
    pub fn synthesis(&self) -> Result<Synthesis> {
        self.read(synthesis)
    }

    pub fn coverage(&self) -> Result<Coverage> {
        self.read(coverage)
    }
}

pub(super) fn coverage(db: &Db) -> Result<Coverage> {
    Ok(Coverage {
        components: progress::counts(
            db,
            concat!(
                "SELECT state,count(*) FROM (SELECT ",
                components::component_coverage!(),
                " AS state FROM component c) GROUP BY state"
            ),
        )?,
        paths: progress::counts(
            db,
            concat!(
                "SELECT state,count(*) FROM (SELECT ",
                paths::path_coverage!(),
                " AS state FROM path p) GROUP BY state"
            ),
        )?,
        deferred: count(
            db,
            "SELECT count(*) FROM deferred d WHERE NOT EXISTS (SELECT 1 FROM deferred_settlement WHERE deferred=d.id)",
            Vec::new(),
        )?,
        open_premises: count(
            db,
            concat!(
                "SELECT count(*) FROM premise p WHERE ",
                premises::open_premise!()
            ),
            Vec::new(),
        )?,
        contracts: progress::counts(
            db,
            "SELECT state,count(*) FROM contract_standing GROUP BY state",
        )?,
        unchallenged: count(
            db,
            concat!(
                "SELECT count(*) FROM guarantee g WHERE ",
                components::load_bearing!("g.component", "g.kind"),
                " AND ",
                unchallenged!("guarantee", "g.id"),
                " AND NOT EXISTS (SELECT 1 FROM contract_standing k JOIN unit_invariant i ON \
                 i.unit=k.unit WHERE k.state='supported' AND i.guarantee=g.id)"
            ),
            Vec::new(),
        )?,
    })
}

pub(super) fn synthesis(db: &Db) -> Result<Synthesis> {
    let mut synthesis = Synthesis::default();
    for (kind, outcome, count) in rows(
        db,
        concat!(
            "SELECT u.kind,",
            unit_outcome!(),
            " AS outcome,count(*) FROM plan_unit u GROUP BY u.kind,outcome"
        ),
        Vec::new(),
        |row| {
            Ok((
                enum_column::<UnitKind>(row, 0)?,
                skyhook::db::optional_enum_column::<Outcome>(row, 1)?,
                row.get::<i64>(2)?.max(0) as u64,
            ))
        },
    )? {
        let tally = synthesis.units.entry(kind).or_default();
        match outcome {
            Some(outcome) => {
                tally.ended.insert(outcome, count);
            }
            None => tally.open = count,
        }
    }
    // Coverage is reconciled only once every unit ended: only then would the plan show
    // the review complete, and only then is the per-path check worth its cost.
    synthesis.standing = match (synthesis.total(), synthesis.open()) {
        (0, _) => PlanStanding::NotPlanned,
        (_, 0) => PlanStanding::Ended {
            unchecked: rows(
                db,
                concat!(
                    "SELECT p.id FROM path p WHERE ",
                    paths::path_coverage!(),
                    "='unchecked' ORDER BY p.id"
                ),
                Vec::new(),
                |row| Ok(PathId(row.get(0)?)),
            )?,
            uninspected: rows(
                db,
                concat!(
                    "SELECT c.id FROM component c WHERE ",
                    components::component_coverage!(),
                    "='reached' ORDER BY c.id"
                ),
                Vec::new(),
                |row| Ok(ComponentId(row.get(0)?)),
            )?,
        },
        _ => PlanStanding::InProgress,
    };
    for phase in Phase::ALL {
        if phase.stage() > Phase::Check.stage() {
            synthesis.findings.insert(phase, queues::tally(db, phase)?);
        }
    }
    Ok(synthesis)
}

#[cfg(test)]
mod tests {
    use super::{
        super::tests::{
            answerer, component, contributes, fixture, follow_up_unit, new_flaw, path, queued,
            reviewed, source, text,
        },
        *,
    };

    /// Every unit ended does not make the review complete while coverage still owes it:
    /// a flaw recorded after its paths were reviewed is named, a membership change plans a
    /// fresh review, and a file moved into a swept component plans another sweep.
    #[test]
    fn changes_that_leave_coverage_undone_plan_units_and_the_synthesis_names_them() {
        let (_directory, store, actor) = fixture();
        let decoder = component(&store, &actor, "decoder");
        let tooling = component(&store, &actor, "tooling");
        let stream = path(&store, &actor, "stream", &[decoder]);
        reviewed(&store, &actor);
        let synthesis = store.synthesis().unwrap();
        assert!(synthesis.complete(), "{}", synthesis.line());

        let late = store.add_flaw(&actor, &new_flaw(decoder, "late")).unwrap();
        let synthesis = store.synthesis().unwrap();
        assert_eq!(
            synthesis.standing,
            PlanStanding::Ended {
                unchecked: vec![stream],
                uninspected: Vec::new(),
            }
        );
        assert!(
            !synthesis.complete()
                && synthesis
                    .line()
                    .contains(&format!("paths {stream} unchecked")),
            "{}",
            synthesis.line()
        );
        contributes(&store, &actor, late, stream);
        assert!(store.synthesis().unwrap().complete());

        let chunked = source(&store, &actor, "chunked", Trust::Unauthenticated);
        store
            .change_path_sources(&actor, stream, &[chunked], &[])
            .unwrap();
        store
            .change_path_sources(&actor, stream, &[chunked], &[])
            .unwrap();
        assert_eq!(
            queued(&store, Phase::PathReview).len(),
            1,
            "one review per change, none for a restatement"
        );

        store
            .add_component(
                &actor,
                &NewComponent {
                    name: text("decoder"),
                    description: text("fixture component"),
                    files: vec![text("tooling/lib.c")],
                },
            )
            .unwrap();
        let [WorkSubject::Unit(sweep)] = queued(&store, Phase::Sweep)[..] else {
            panic!("the moved-in file plans a sweep of the reached component");
        };
        assert!(
            matches!(store.unit(sweep).unwrap().work, UnitWork::Sweep { component } if component == decoder)
        );
        assert_eq!(
            store.dossier(tooling).unwrap().component.coverage,
            ComponentCoverage::Mapped
        );
    }

    /// Paths and their reach plan the review; outcomes settle units and the synthesis
    /// reports what ended with a limitation. Coverage counts components and paths by how far
    /// review reached them.
    #[test]
    fn plan_units_take_outcomes_and_the_synthesis_reports_them() {
        let (_directory, store, actor) = fixture();
        let decoder = component(&store, &actor, "decoder");
        let tooling = component(&store, &actor, "tooling");
        let entry = path(&store, &actor, "entry", &[decoder]);
        let units = |phase| queued(&store, phase);
        let [WorkSubject::Unit(sweep)] = units(Phase::Sweep)[..] else {
            panic!("one sweep planned, of the reached component only");
        };
        assert!(
            matches!(store.unit(sweep).unwrap().work, UnitWork::Sweep { component } if component == decoder)
        );
        // The path's review is owed but waits for the sweep of the component it reaches.
        assert!(units(Phase::PathReview).is_empty());
        let synthesis = store.synthesis().unwrap();
        assert_eq!(synthesis.tally(UnitKind::PathReview).open, 1);
        // The agent that planned a unit never answers it, and a unit ends once.
        assert!(matches!(
            store.record_outcome(&actor, sweep, Outcome::Completed, &text("swept"), &[]),
            Err(Error::OwnUnit { .. })
        ));
        store
            .record_outcome(
                &answerer(&actor),
                sweep,
                Outcome::Completed,
                &text("swept"),
                &[],
            )
            .unwrap();
        assert!(matches!(
            store.record_outcome(
                &answerer(&actor),
                sweep,
                Outcome::Blocked,
                &text("again"),
                &[]
            ),
            Err(Error::UnitEnded { .. })
        ));
        let [WorkSubject::Unit(review)] = units(Phase::PathReview)[..] else {
            panic!("the path's review is ready once its component is swept");
        };
        assert!(
            matches!(store.unit(review).unwrap().work, UnitWork::PathReview { path } if path == entry)
        );
        store
            .record_outcome(
                &actor,
                review,
                Outcome::Blocked,
                &text("tables missing"),
                &[],
            )
            .unwrap();
        assert!(units(Phase::Sweep).is_empty() && units(Phase::PathReview).is_empty());

        // A leaf adds a follow-up, which its coordinator's pool reads.
        let follow_up = follow_up_unit(None, None, Vec::new());
        let follow_up = store.plan_unit(&answerer(&actor), &follow_up).unwrap();
        assert_eq!(units(Phase::FollowUp), [WorkSubject::Unit(follow_up)]);
        let synthesis = store.synthesis().unwrap();
        assert_eq!(
            synthesis.line(),
            "planned review in progress: 1 of 3 units open; 1 blocked"
        );
        store
            .record_outcome(&actor, follow_up, Outcome::Completed, &text("bounded"), &[])
            .unwrap();
        assert_eq!(
            store.synthesis().unwrap().line(),
            "planned review complete: 3 units; 1 blocked"
        );
        // A question ended deferred leaves its mechanism unchallenged.
        let unasked = store
            .plan_unit(
                &actor,
                &UnitWork::Question {
                    question: text("can the decoder's retry exceed its staging bound?"),
                    stopping: text(
                        "a counterexample, a proof over every return site, or a premise",
                    ),
                    invariants: Vec::new(),
                    check: None,
                },
            )
            .unwrap();
        store
            .record_outcome(
                &actor,
                unasked,
                Outcome::Deferred,
                &text("unchallenged"),
                &[],
            )
            .unwrap();
        assert_eq!(
            store.synthesis().unwrap().line(),
            "planned review complete: 4 units; 1 blocked, 1 deferred"
        );
        // A path reaching nothing is unreached; a component a path reaches is reached until
        // its sweep ends.
        path(&store, &actor, "admin", &[]);
        let parser = component(&store, &actor, "parser");
        path(&store, &actor, "upload", &[parser]);
        let coverage = store.coverage().unwrap();
        assert_eq!(
            [
                ComponentCoverage::Inspected,
                ComponentCoverage::Reached,
                ComponentCoverage::Mapped
            ]
            .map(|state| coverage.components_in(state)),
            [1, 1, 1]
        );
        assert_eq!(
            [
                PathCoverage::Checked,
                PathCoverage::Unchecked,
                PathCoverage::Unreached
            ]
            .map(|state| coverage.paths_in(state)),
            [2, 0, 1]
        );
        assert_eq!(
            store.dossier(tooling).unwrap().component.coverage,
            ComponentCoverage::Mapped
        );
    }
}

//! Work each phase owes, derived from the graph: paths owing a sweep, premises a settling
//! look, flaws a chain, candidates an assessment, a review or a test, and root flaws a report.
//! A subject is offered while it owes the phase, no task of it is live, and fewer than two of
//! its tasks began at its current basis, the latest event of the phase's `bearing` on it; once
//! two did and none is live it is exhausted until the basis moves, as is work the scheduler
//! spent at that basis this run. A queue lists what is offered and ready now; its owed listing
//! is everything offered, ready or not, with what is exhausted.

use std::collections::BTreeMap;

use libsql::Row;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook::db::{Db, enum_column, params};

use super::*;

use crate::store::bearing::{basis, bearing, chain_premises};

/// Task subject `$subject` is node `$id`, or a node merged into it since its task began.
macro_rules! works {
    ($subject:expr, $id:expr) => {
        concat!(
            $subject,
            " IN (SELECT ",
            $id,
            " UNION ALL SELECT id FROM node WHERE merged_into=",
            $id,
            ")"
        )
    };
}
pub(in crate::store) use works;

/// A task of `$phase` on node `$id` is live.
macro_rules! task_live {
    ($phase:tt, $id:expr) => {
        concat!(
            "EXISTS (SELECT 1 FROM task_node lt JOIN task_state ls ON ls.task=lt.task WHERE \
             lt.phase='",
            $phase,
            "' AND ",
            works!("lt.node", $id),
            " AND ls.live)"
        )
    };
}

/// `$phase`'s work on node `$id` is not exhausted at its current basis: fewer than two of its
/// tasks began there, and the scheduler has not [`Spent`] it there this run.
macro_rules! retryable {
    ($phase:tt, $id:expr) => {
        concat!(
            "(NOT EXISTS (SELECT 1 FROM json_each(?99) sw WHERE sw.value->>0='",
            $phase,
            "' AND sw.value->>1=",
            $id,
            " AND sw.value->>2=",
            basis!($phase, $id),
            ") AND (SELECT count(*) FROM task_node rt WHERE rt.phase='",
            $phase,
            "' AND rt.node=",
            $id,
            " AND rt.basis=",
            basis!($phase, $id),
            ")<2)"
        )
    };
}

/// `$phase` owes work on live node `$id` (`$owed`) that is not exhausted: a task of it is
/// live, or it may be tried again. Only such work holds other work back.
macro_rules! pending {
    ($phase:tt, $id:expr, $owed:expr) => {
        concat!(
            "(EXISTS (SELECT 1 FROM node pn WHERE pn.id=",
            $id,
            " AND pn.merged_into IS NULL) AND ",
            $owed,
            " AND (",
            task_live!($phase, $id),
            " OR ",
            retryable!($phase, $id),
            "))"
        )
    };
}

/// A phase's statements over its live subjects `n`, each row the key, title, basis and the
/// pending test it resumes: the page of offered entries that are `$ready`, past `?1`, limit
/// `?2`; every `$owed` subject without a live task, flagged when exhausted; and, for a bundled
/// phase, the offered and ready entries `$naming` selects from the nodes bound as `?1`.
macro_rules! phase_sql {
    ($phase:tt, $kind:literal, $owed:expr, $ready:expr, $pending:expr, $naming:expr) => {
        phase_sql!(
            @ concat!(
                "SELECT n.id,n.title,",
                basis!($phase, "n.id"),
                ",",
                $pending,
                " FROM node n WHERE n.kind='",
                $kind,
                "' AND n.merged_into IS NULL AND "
            ),
            concat!(
                "SELECT n.id,n.title,",
                basis!($phase, "n.id"),
                ",NOT ",
                retryable!($phase, "n.id"),
                " FROM node n WHERE n.kind='",
                $kind,
                "' AND n.merged_into IS NULL AND "
            ),
            concat!($owed, " AND NOT ", task_live!($phase, "n.id")),
            retryable!($phase, "n.id"),
            $ready,
            $naming
        )
    };
    (@ $select:expr, $flagged:expr, $unstarted:expr, $retryable:expr, $ready:expr, $naming:expr) => {
        Sql {
            page: concat!($select, "n.id>?1 AND ", $unstarted, " AND ", $retryable, " AND ",
                $ready, " ORDER BY n.id LIMIT ?2"),
            owed: concat!($flagged, $unstarted, " ORDER BY n.id"),
            naming: concat!($select, "n.id IN (", $naming, ") AND ", $unstarted, " AND ",
                $retryable, " AND ", $ready, " ORDER BY n.id"),
        }
    };
}

/// A phase's statements.
struct Sql {
    page: &'static str,
    owed: &'static str,
    naming: &'static str,
}

mod candidates;
mod reports;
mod structure;

pub(super) use self::{
    candidates::review_owed,
    reports::{current_finding, reported},
};
use self::{candidates::*, reports::*, structure::*};

const fn sql(phase: Phase) -> Sql {
    match phase {
        Phase::Sweep => SWEEP,
        Phase::Settle => SETTLE,
        Phase::Chain => CHAIN,
        Phase::Assess => ASSESS,
        Phase::Review => REVIEW,
        Phase::Test => TEST,
        Phase::Report => REPORT,
    }
}

/// An entry of a phase's queue: its subject, what its work rests on, the nodes it is about
/// and, for a test resuming a pending one, that test.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct QueueEntry {
    pub node: NodeId,
    pub title: String,
    pub basis: EventId,
    /// What the work is about: a premise and the flaws depending on it, a flaw, or a
    /// candidate, its chain's flaws and the premises they and its path rely on.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub about: Vec<NodeId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<TestId>,
}

impl Keyed for QueueEntry {
    fn key(&self) -> i64 {
        self.node.0
    }
}

/// Work the scheduler launched twice at one basis this run, though perhaps no task of it began:
/// exhausted while its phase owes it at that basis, as two tasks begun there would leave it.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SpentWork {
    pub phase: Phase,
    pub node: NodeId,
    pub basis: EventId,
}

/// The work the scheduler spent this run, as `[phase, node, basis]` rows. [`retryable`] reads
/// them from the parameter numbered past every statement's own, `?99`; a statement leaving it
/// unbound, or the default, reads none spent.
pub(super) struct Spent(libsql::Value);

impl Default for Spent {
    fn default() -> Self {
        Self(libsql::Value::Null)
    }
}

impl Spent {
    const PARAMETER: usize = 99;

    pub(super) fn of(spent: &[SpentWork]) -> Result<Self> {
        let rows: Vec<(Phase, i64, i64)> = (spent.iter())
            .map(|work| (work.phase, work.node.0, work.basis.0))
            .collect();
        Ok(Self(json_list(&rows)?.into()))
    }

    /// `params` with the spent work bound to its parameter.
    fn bound(&self, mut params: Vec<libsql::Value>) -> Vec<libsql::Value> {
        params.resize(Self::PARAMETER - 1, libsql::Value::Null);
        params.push(self.0.clone());
        params
    }
}

/// The entries `sql` lists, each with what it is about.
fn listed(db: &Db, sql: &'static str, params: Vec<libsql::Value>) -> Result<Vec<QueueEntry>> {
    let mut entries = rows(db, sql, params, |row| {
        Ok(QueueEntry {
            node: NodeId(row.get(0)?),
            title: row.get(1)?,
            basis: EventId(row.get(2)?),
            about: Vec::new(),
            pending: row.get::<Option<i64>>(3)?.map(TestId),
        })
    })?;
    let nodes: Vec<NodeId> = entries.iter().map(|entry| entry.node).collect();
    let mut about = about(db, &nodes)?;
    for entry in &mut entries {
        entry.about = about
            .remove(&entry.node)
            .map(|(_, about)| about)
            .unwrap_or_default();
    }
    Ok(entries)
}

/// A subject `phase` owes with no task of it live.
pub(super) struct Owed {
    pub(super) node: NodeId,
    pub(super) title: String,
    pub(super) basis: EventId,
    /// Two tasks began at its current basis, or the scheduler spent it there: it is not offered
    /// until the basis moves.
    pub(super) exhausted: bool,
}

/// Every subject `phase` owes with no task of it live, offered or exhausted, ready or not.
pub(super) fn owed(db: &Db, phase: Phase, spent: &Spent) -> Result<Vec<Owed>> {
    rows(db, sql(phase).owed, spent.bound(Vec::new()), |row: &Row| {
        Ok(Owed {
            node: NodeId(row.get(0)?),
            title: row.get(1)?,
            basis: EventId(row.get(2)?),
            exhausted: row.get(3)?,
        })
    })
}

/// The next page of `phase` work that can start now: a subject whose prerequisites are still
/// open waits off the page while it stays owed.
pub(super) fn page(
    db: &Db,
    phase: Phase,
    page: Page,
    spent: &Spent,
) -> Result<Listing<QueueEntry>> {
    let listed = listed(
        db,
        sql(phase).page,
        spent.bound(params![page.after, page.limit()]),
    )?;
    Ok(Listing::keyset(listed, page))
}

/// The offered and ready entries of bundled `phase` about one of `nodes`, in key order.
pub(super) fn naming(
    db: &Db,
    phase: Phase,
    nodes: &[NodeId],
    spent: &Spent,
) -> Result<Vec<QueueEntry>> {
    listed(
        db,
        sql(phase).naming,
        spent.bound(params![json_list(nodes)?]),
    )
}

/// The kind of each of `nodes` and what work on it is about: a premise and the flaws depending
/// on it, a flaw, or a candidate, its chain's flaws and the premises its chain rests on.
pub(super) fn about(
    db: &Db,
    nodes: &[NodeId],
) -> Result<BTreeMap<NodeId, (NodeKind, Vec<NodeId>)>> {
    let mut about: BTreeMap<NodeId, (NodeKind, Vec<NodeId>)> = rows(
        db,
        "SELECT n.id,n.kind FROM json_each(?1) j JOIN node n ON n.id=j.value",
        params![json_list(nodes)?],
        |row| {
            let (node, kind) = (NodeId(row.get(0)?), enum_column::<NodeKind>(row, 1)?);
            let own = matches!(
                kind,
                NodeKind::Premise | NodeKind::Flaw | NodeKind::Candidate
            );
            Ok((node, (kind, own.then_some(node).into_iter().collect())))
        },
    )?
    .into_iter()
    .collect();
    for (node, record) in rows(
        db,
        "SELECT fp.premise,fp.flaw FROM json_each(?1) j JOIN flaw_premise fp ON \
         fp.premise=j.value UNION ALL SELECT cf.candidate,cf.flaw FROM json_each(?1) j JOIN \
         candidate_flaw cf ON cf.candidate=j.value UNION ALL SELECT cf.candidate,fp.premise FROM \
         json_each(?1) j JOIN candidate_flaw cf ON cf.candidate=j.value JOIN flaw_premise fp ON \
         fp.flaw=cf.flaw UNION ALL SELECT c.node,pp.premise FROM json_each(?1) j JOIN candidate c \
         ON c.node=j.value JOIN path_premise pp ON pp.path=c.path",
        params![json_list(nodes)?],
        |row| Ok((NodeId(row.get(0)?), NodeId(row.get(1)?))),
    )? {
        if let Some((_, about)) = about.get_mut(&node) {
            about.push(record);
        }
    }
    Ok(about)
}

/// The phase and subject of each live task, a merged one as the node it was merged into.
pub(super) fn live_subjects(db: &Db) -> Result<Vec<(Phase, NodeId)>> {
    rows(
        db,
        "SELECT tn.phase,coalesce(n.merged_into,n.id) FROM task_node tn JOIN task_state s ON \
         s.task=tn.task JOIN node n ON n.id=tn.node WHERE s.live ORDER BY tn.task,tn.node",
        Vec::new(),
        |row| Ok((enum_column(row, 0)?, NodeId(row.get(1)?))),
    )
}

#[cfg(test)]
pub(super) mod tests {
    use skyhook::{Bounded, job::JobState};

    use super::{
        super::tests::{
            HIGH, assess, begun, child_of, end_job, fixture, premise, scenario, sweep, test, text,
            verdict,
        },
        *,
    };

    /// Everything `phase` owes that can start now, in key order.
    pub(in crate::store) fn queued(store: &Store, phase: Phase) -> Vec<NodeId> {
        let spent = &Spent::default();
        Listing::drain(|page| store.read(|db| super::page(db, phase, page, spent)))
            .unwrap()
            .into_iter()
            .map(|entry| entry.node)
            .collect()
    }

    pub(in crate::store) fn status(store: &Store, id: CandidateId) -> Status {
        let NodeView::Candidate(view) = store.node(id.into()).unwrap() else {
            panic!("a candidate")
        };
        view.status
    }

    /// Two tasks of `work` whose jobs end without the work exhaust its first subject in its
    /// phase: no longer offered, it is listed exhausted.
    pub(in crate::store) fn exhaust(store: &Store, work: &Begin) {
        let phase = work.phase();
        let subject = match queued(store, phase).first() {
            Some(first) => *first,
            None => panic!("{phase} offers nothing to exhaust"),
        };
        for _ in 0..2 {
            let (_, job, _) = begun(store, work);
            end_job(store, job);
        }
        assert!(
            !queued(store, phase).contains(&subject),
            "{phase} still offers {subject}"
        );
        assert!(
            (store.synthesis(&[]).unwrap().exhausted.iter())
                .any(|entry| entry.phase == phase && entry.node == subject),
            "{phase} lists {subject} exhausted"
        );
    }

    /// Spent work is exhausted only in its phase and at the basis it was spent on, and holds
    /// back nothing: a spent sweep releases the assessment waiting on it, and a spent
    /// assessment is offered again once its candidate changes.
    #[test]
    fn spent_work_is_exhausted_only_at_its_basis() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        let started = |spent: &[SpentWork]| {
            let page = store
                .startable(&[], spent, &Tests::default(), None, Bounded::new::<10>())
                .unwrap();
            (page.items.into_iter())
                .map(|item| SpentWork {
                    phase: item.phase,
                    node: item.entry.node,
                    basis: item.entry.basis,
                })
                .collect::<Vec<_>>()
        };
        let [sweep] = started(&[])[..] else {
            panic!("the sweep alone is offered")
        };
        assert_eq!(
            (sweep.phase, sweep.node),
            (Phase::Sweep, scenario.path.into())
        );
        let [assess] = started(&[sweep])[..] else {
            panic!("the spent sweep releases the assessment")
        };
        assert_eq!(
            (assess.phase, assess.node),
            (Phase::Assess, scenario.candidate.into())
        );
        assert!(started(&[sweep, assess]).is_empty());
        let exhausted = store.synthesis(&[sweep, assess]).unwrap().exhausted;
        assert_eq!(
            exhausted
                .iter()
                .map(|entry| (entry.phase, entry.node))
                .collect::<Vec<_>>(),
            [(Phase::Sweep, sweep.node), (Phase::Assess, assess.node)]
        );
        let edit = Edit {
            title: None,
            body: Some(text("narrower than stated")),
            detail: CandidateEdit::default(),
        };
        let change = Upsert::Candidate(Change::Edit(scenario.candidate, edit));
        store.upsert(&actor, &change).unwrap();
        let [moved] = started(&[sweep, assess])[..] else {
            panic!("the changed candidate's assessment is offered")
        };
        assert_eq!((moved.phase, moved.node), (Phase::Assess, assess.node));
        assert!(moved.basis > assess.basis);
    }

    /// What a phase's tasks on a subject record, by their agents or those beneath them, never
    /// moves its basis: two tasks that end owed exhaust it whatever they recorded, until another
    /// agent changes what it rests on. A pending test is resumed, so its record moves it.
    #[test]
    fn a_tasks_own_records_never_move_its_basis() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        let id = scenario.candidate;
        sweep(&store, scenario.path);
        assess(&store, &actor, id, HIGH);
        let rely = |agent: &AgentId| {
            let premise = premise(&store, &actor, PremiseScope::Code);
            let holds = StatusChange::Premise(premise, PremiseStatus::Holds);
            store.set_status(&actor, holds, &text("checked")).unwrap();
            let link = Link::FlawPremise {
                flaw: scenario.flaw,
                premise,
            };
            store.link(agent, &[link]).unwrap();
        };
        for _ in 0..2 {
            let (reviewer, job, _) = begun(&store, &Begin::Review(vec![id]));
            let (helper, _) = child_of(&store, &reviewer, JobState::Running);
            rely(&reviewer);
            rely(&helper);
            end_job(&store, job);
        }
        assert!(queued(&store, Phase::Review).is_empty());
        assert_eq!(store.synthesis(&[]).unwrap().exhausted(Phase::Review), 1);
        rely(&actor);
        assert_eq!(queued(&store, Phase::Review), [NodeId::from(id)]);

        verdict(&store, &actor, id, Verdict::Validated);
        for _ in 0..2 {
            let (prover, job, _) = begun(&store, &Begin::Test(vec![id]));
            test(&store, &prover, id, TestResult::Pending);
            end_job(&store, job);
        }
        assert_eq!(queued(&store, Phase::Test), [NodeId::from(id)]);
    }

    /// Every phase's statements and the views deriving standing seek the log, the task log and
    /// the graph by node: the only scans are of a per-subject result, the bound lists, the run's
    /// one row, and the environments prepared for any candidate.
    #[test]
    fn phase_statements_seek_by_node() {
        let (_session, store, _) = fixture();
        let mut statements = vec![
            "SELECT * FROM candidate_state WHERE candidate=?1",
            "SELECT * FROM finding_scenario WHERE root=?1",
        ];
        for phase in Phase::ALL {
            let sql = sql(phase);
            statements.extend([sql.page, sql.owed, crate::store::bearing::lookup(phase)]);
            if phase.bundled() {
                statements.push(sql.naming);
            }
        }
        let allowed = [
            "SCAN ob",
            "SCAN cp",
            "SCAN vp",
            "SCAN tp",
            "SCAN sw VIRTUAL TABLE",
            "SCAN json_each VIRTUAL TABLE",
            "SCAN run",
            "SCAN CONSTANT ROW",
            "SCAN environment USING INDEX environment_unscoped",
        ];
        let scans = store
            .read(|db| {
                let mut scans = Vec::new();
                for sql in &statements {
                    let plan = rows(db, format!("EXPLAIN QUERY PLAN {sql}"), Vec::new(), |row| {
                        Ok(row.get::<String>(3)?)
                    })?;
                    scans.extend(
                        (plan.into_iter())
                            .filter(|step| step.starts_with("SCAN "))
                            .filter(|step| !allowed.iter().any(|scan| step.starts_with(scan)))
                            .map(|step| format!("{step} in {sql}")),
                    );
                }
                Ok(scans)
            })
            .unwrap();
        assert!(scans.is_empty(), "{scans:#?}");
    }
}

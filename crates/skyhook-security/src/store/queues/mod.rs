//! Work each phase owes, derived from the graph: paths owing a sweep or a probe, states a check
//! across calls, premises a settling look, flaws a chain, candidates an assessment, a review or a test, and root flaws a report.
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

use crate::store::bearing::{action_premises, basis, bearing, chain_premises};

/// A task of `$phase` on node `$id`, or on a node merged into it since the task began, is live:
/// `$id` is in the list of the phase's live subjects, which a statement reads once.
macro_rules! task_live {
    ($phase:tt, $id:expr) => {
        concat!(
            "(",
            $id,
            " IN (SELECT lt.node FROM live lt WHERE lt.phase='",
            $phase,
            "'))"
        )
    };
}

/// `$phase`'s work on node `$id` is not exhausted at its current basis: fewer than two of its
/// tasks began there, and the scheduler has not [`Spent`] it there this run. The phase's spent
/// work is read once, as a list each subject's basis is looked up in.
macro_rules! retryable {
    ($phase:tt, $id:expr) => {
        concat!(
            "((",
            $id,
            ",",
            basis!($phase, $id),
            ") NOT IN (SELECT sw.node,sw.basis FROM spent sw WHERE sw.phase='",
            $phase,
            "') AND (SELECT count(*) FROM task_node rt WHERE rt.phase='",
            $phase,
            "' AND rt.node=",
            $id,
            " AND rt.basis=",
            basis!($phase, $id),
            ")<2)"
        )
    };
}

/// `$phase`'s work on node `$id` is not exhausted: a task of it is live, or it may be tried
/// again.
macro_rules! unexhausted {
    ($phase:tt, $id:expr) => {
        concat!(
            "(",
            task_live!($phase, $id),
            " OR ",
            retryable!($phase, $id),
            ")"
        )
    };
}

/// `$phase` owes work on live node `$id` (`$owed`) that is not exhausted. Only such work holds
/// other work back.
macro_rules! pending {
    ($phase:tt, $id:expr, $owed:expr) => {
        concat!(
            "(EXISTS (SELECT 1 FROM node pn WHERE pn.id=",
            $id,
            " AND pn.merged_into IS NULL) AND ",
            $owed,
            " AND ",
            unexhausted!($phase, $id),
            ")"
        )
    };
}

/// A phase's statements over its live subjects `n`, each row the key, title, basis and the
/// pending test it resumes: the page of offered entries that are `$ready`, past `?1` up to `?2`;
/// every `$owed` subject without a live task, flagged when exhausted; and, for a bundled phase,
/// the offered and ready entries `$naming` selects from the nodes bound as `?1`, at least every
/// one about one of them, and those `$bundle` selects as the bundle of the entry bound as `?1`.
/// Each statement but the whole listing reads the facts of only the root groups of the subjects
/// it may list.
macro_rules! phase_sql {
    ($phase:tt, $kind:literal, $owed:expr, $ready:expr, $pending:expr, $naming:expr,
        $bundle:expr) => {
        phase_sql!(
            @ $kind,
            concat!(
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
            $naming,
            $bundle
        )
    };
    (@ $kind:literal, $select:expr, $flagged:expr, $unstarted:expr, $retryable:expr, $ready:expr,
        $naming:expr, $bundle:expr) => {
        Sql {
            page: concat!(
                relations!(concat!("SELECT id FROM node WHERE kind='", $kind,
                    "' AND merged_into IS NULL AND id>?1 AND id<=?2")),
                $select, "n.id>?1 AND n.id<=?2 AND ", $unstarted, " AND ",
                $retryable, " AND ", $ready, " ORDER BY n.id"),
            owed: concat!(relations!(), $flagged, $unstarted, " ORDER BY n.id"),
            naming: concat!(relations!($naming), $select, "n.id IN (", $naming, ") AND ", $unstarted,
                " AND ", $retryable, " AND ", $ready, " ORDER BY n.id"),
            bundle: concat!(relations!($bundle), $select, "n.id IN (", $bundle, ") AND ", $unstarted,
                " AND ", $retryable, " AND ", $ready, " ORDER BY n.id"),
        }
    };
}

/// The relations a phase statement reads once, each when it first reads it: the [`Spent`] work
/// bound as `?99`, which [`retryable`] lists by phase, the subjects of live
/// tasks, which [`task_live`] lists by phase, and the facts selecting tests, of the
/// root groups of the candidates and the root flaws among `$subjects`, or of every group.
macro_rules! relations {
    ($($subjects:expr)?) => {
        concat!(
            "WITH spent(phase,node,basis) AS MATERIALIZED (SELECT value->>0,value->>1,value->>2 \
             FROM json_each(?99)),live AS MATERIALIZED (",
            $crate::store::tasks::live!(),
            "),",
            tested!($(concat!(
                "SELECT root FROM candidate_root WHERE candidate IN (", $subjects,
                ") UNION SELECT node FROM flaw WHERE node IN (", $subjects, ")"
            ))?),
            " "
        )
    };
}

/// A phase's statements.
struct Sql {
    page: &'static str,
    owed: &'static str,
    naming: &'static str,
    bundle: &'static str,
}

mod candidates;
mod reports;
mod structure;

pub use self::candidates::Behind;
use self::{candidates::*, reports::*, structure::*};
pub(super) use self::{
    candidates::{behind, review_owed},
    reports::{current_finding, reported},
    structure::{check_stale, probed, served},
};

const fn sql(phase: Phase) -> Sql {
    match phase {
        Phase::Sweep => SWEEP,
        Phase::Probe => PROBE,
        Phase::Check => CHECK,
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
    /// candidate, its chain's flaws and the premises they and its paths rely on.
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

/// The work the scheduler spent this run, as `[phase, node, basis]` rows bound to the parameter
/// numbered past every statement's own, `?99`, which [`relations`] decodes; a statement leaving it
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

/// The first `limit` entries `sql` lists, each with what it is about.
fn listed(
    db: &Db,
    sql: &'static str,
    params: Vec<libsql::Value>,
    limit: usize,
) -> Result<Vec<QueueEntry>> {
    let mut entries = first_rows(db, sql, params, limit, |row| {
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

fn owed_row(row: &Row) -> Result<Owed> {
    Ok(Owed {
        node: NodeId(row.get(0)?),
        title: row.get(1)?,
        basis: EventId(row.get(2)?),
        exhausted: row.get(3)?,
    })
}

/// Every subject `phase` owes with no task of it live, offered or exhausted, ready or not.
pub(super) fn owed(db: &Db, phase: Phase, spent: &Spent) -> Result<Vec<Owed>> {
    rows(db, sql(phase).owed, spent.bound(Vec::new()), owed_row)
}

/// Whether `phase` owes any subject with no task of it live. Each of its other statements lists
/// only such subjects, and costs more to compile for what it adds.
pub(super) fn owes(db: &Db, phase: Phase, spent: &Spent) -> Result<bool> {
    exists(db, sql(phase).owed, spent.bound(Vec::new()))
}

/// The next page of `phase` work that can start now: a subject whose prerequisites are still
/// open waits off the page while it stays owed. The subjects past the cursor are read in
/// windows, the first as wide as the page and each next twice the last, so a page reads the
/// facts of the groups it reaches alone.
pub(super) fn page(
    db: &Db,
    phase: Phase,
    page: Page,
    spent: &Spent,
) -> Result<Listing<QueueEntry>> {
    let (mut after, mut width, mut items) = (page.after, page.limit(), Vec::new());
    loop {
        let subjects: Vec<i64> = first_rows(
            db,
            "SELECT id FROM node WHERE kind=?1 AND merged_into IS NULL AND id>?2 ORDER BY id",
            params![phase.subject(), after],
            width,
            |row| Ok(row.get(0)?),
        )?;
        let end = (subjects.len() == width).then(|| subjects[width - 1]);
        let wanted = page.limit() - items.len();
        let window = params![after, end.unwrap_or(i64::MAX)];
        items.extend(listed(db, sql(phase).page, spent.bound(window), wanted)?);
        match end {
            Some(end) if items.len() < page.limit() => (after, width) = (end, width * 2),
            _ => return Ok(Listing::keyset(items, page)),
        }
    }
}

/// The offered and ready entries of bundled `phase` in `head`'s bundle, `head` among them, in
/// key order.
pub(super) fn bundle(
    db: &Db,
    phase: Phase,
    head: NodeId,
    spent: &Spent,
) -> Result<Vec<QueueEntry>> {
    listed(
        db,
        sql(phase).bundle,
        spent.bound(params![json_list(&[head])?]),
        usize::MAX,
    )
}

/// The offered and ready entries of bundled `phase` about one of `nodes`, perhaps more, in key
/// order.
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
        usize::MAX,
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
        concat!(
            "SELECT fp.premise,fp.flaw FROM json_each(?1) j JOIN flaw_premise fp ON \
             fp.premise=j.value UNION ALL SELECT cf.candidate,cf.flaw FROM json_each(?1) j JOIN \
             candidate_flaw cf ON cf.candidate=j.value UNION ALL SELECT cf.candidate,fp.premise \
             FROM json_each(?1) j JOIN candidate_flaw cf ON cf.candidate=j.value JOIN \
             flaw_premise fp ON fp.flaw=cf.flaw UNION ALL SELECT DISTINCT pa.candidate,pp.premise \
             FROM json_each(?1) j JOIN ",
            action_premises!("pa.candidate=j.value")
        ),
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
        concat!(
            "SELECT phase,node FROM (",
            tasks::live!(),
            ") ORDER BY task,node"
        ),
        Vec::new(),
        |row| Ok((enum_column(row, 0)?, NodeId(row.get(1)?))),
    )
}

#[cfg(test)]
pub(super) mod tests {
    use skyhook::{Bounded, job::JobState};

    use super::{
        super::tests::{
            HIGH, NONE, Step, assess, begun, child_of, decide, end_job, fixture, last_event, marks,
            plan, premise, scenario, sweep, test, text, verdict,
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
        let tasks: Vec<TaskId> = (0..2)
            .map(|_| {
                let (_, job, task) = begun(store, work);
                end_job(store, job);
                task
            })
            .collect();
        assert!(
            !queued(store, phase).contains(&subject),
            "{phase} still offers {subject}"
        );
        assert!(
            (store.synthesis(&[]).unwrap().exhausted.iter())
                .any(|entry| entry.phase == phase && entry.node == subject && entry.tasks == tasks),
            "{phase} lists {subject} exhausted with the tasks that tried it"
        );
    }

    /// Spent work is exhausted only in its phase and at the basis it was spent on, and holds
    /// back nothing: a spent sweep releases the assessment waiting on it, a spent assessment is
    /// offered again once its candidate changes, and once settled releases the review.
    #[test]
    fn spent_work_is_exhausted_only_at_its_basis() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        let started = |spent: &[SpentWork]| {
            let page = store
                .startable(&[], spent, &Live::default(), None, Bounded::new::<10>())
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
        crate::store::tests::assess(&store, scenario.candidate, HIGH);
        let [review] = started(&[sweep, moved])[..] else {
            panic!("work settled at the basis it was spent on releases its next phase")
        };
        assert_eq!((review.phase, review.node), (Phase::Review, assess.node));
        let exhausted = store.synthesis(&[sweep, moved]).unwrap().exhausted;
        assert_eq!(
            (exhausted.iter())
                .map(|entry| (entry.phase, entry.node))
                .collect::<Vec<_>>(),
            [(Phase::Sweep, sweep.node)]
        );
    }

    /// What a review, and a child it launches that begins a review of its own, records of the
    /// results other phases own only proposes them: a premise it fails reads undecided and owes
    /// its settle, a flaw it refutes stays suspected on a live candidate, its disproval does not
    /// stand while what it claims does not, and its reassessment owes an assessment while the
    /// candidate keeps its standing one. Once a settle fails the premise, the review is owed
    /// again and its disproval stands. A new differing proposal owes the assessment afresh,
    /// however many assessments decided earlier ones.
    #[test]
    fn a_reviews_decisions_outside_their_phases_are_proposals() {
        let (_session, store, actor) = fixture();
        let decoder = scenario(&store, &actor, "decoder");
        let (candidate, flaw) = (decoder.candidate, decoder.flaw);
        sweep(&store, decoder.path);
        let premise = premise(&store, &actor, PremiseScope::Code);
        store
            .link(&actor, &[Link::FlawPremise { flaw, premise }])
            .unwrap();
        assess(&store, candidate, HIGH);

        let (reviewer, review, _) = begun(&store, &Begin::Review(vec![candidate]));
        let (child, nested) = child_of(&store, &reviewer, JobState::Running);
        store
            .begin(&child, &Begin::Review(vec![candidate]))
            .unwrap();
        let fails = StatusChange::Premise(premise, PremiseStatus::Fails);
        store
            .set_status(&child, fails, &text("an exhaustive audit"))
            .unwrap();
        let refuted = StatusChange::Flaw(flaw, FlawStatus::Refuted);
        store
            .set_status(&reviewer, refuted, &text("the premise fails"))
            .unwrap();
        store
            .verdict(&reviewer, candidate, Verdict::Disproved, &text("refuted"))
            .unwrap();
        let none = NONE.parse().unwrap();
        store
            .assess(&reviewer, candidate, none, &text("no impact"))
            .unwrap();
        for job in [nested, review] {
            end_job(&store, job);
        }

        let NodeView::Premise(view) = store.node(premise.into()).unwrap() else {
            panic!("a premise")
        };
        let proposed = view.proposed.expect("the child's decision is proposed");
        assert_eq!(
            (view.status, proposed.value, &proposed.agent),
            (PremiseStatus::Undecided, PremiseStatus::Fails, &child)
        );
        let NodeView::Flaw(view) = store.node(flaw.into()).unwrap() else {
            panic!("a flaw")
        };
        assert_eq!(view.status, FlawStatus::Suspected);
        assert_eq!(queued(&store, Phase::Settle), [NodeId::from(premise)]);
        assert_eq!(queued(&store, Phase::Chain), [NodeId::from(flaw)]);
        let NodeView::Candidate(view) = store.node(candidate.into()).unwrap() else {
            panic!("a candidate")
        };
        assert_eq!(
            (view.status, view.assessment.unwrap().severity),
            (Status::Shortlisted, Severity::Critical)
        );
        assert_eq!(
            view.proposed_assessment.unwrap().value.severity,
            Severity::None
        );
        assert_eq!(queued(&store, Phase::Assess), [NodeId::from(candidate)]);

        decide(&store, premise, PremiseStatus::Fails);
        let NodeView::Premise(view) = store.node(premise.into()).unwrap() else {
            panic!("a premise")
        };
        assert_eq!(view.status, PremiseStatus::Fails);
        assert_eq!(store.run_facts().unwrap().summary.owed(Phase::Review), 1);
        assess(&store, candidate, HIGH);
        assert_eq!(queued(&store, Phase::Review), [NodeId::from(candidate)]);
        verdict(&store, candidate, Verdict::Disproved);
        assert_eq!(status(&store, candidate), Status::Disproved);

        let lower = "CVSS:4.0/AV:N/AC:L/AT:N/PR:L/UI:N/VC:L/VI:L/VA:N/SC:N/SI:N/SA:N";
        store
            .assess(&actor, candidate, lower.parse().unwrap(), &text("lower"))
            .unwrap();
        assert_eq!(
            marks(&store, last_event(&store)),
            [(candidate.into(), EventEffect::Disputes)]
        );
        assert_eq!(status(&store, candidate), Status::Disproved);
        assert_eq!(queued(&store, Phase::Assess), [NodeId::from(candidate)]);
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
        assess(&store, id, HIGH);
        let rely = |agent: &AgentId| {
            let premise = premise(&store, &actor, PremiseScope::Code);
            decide(&store, premise, PremiseStatus::Holds);
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
        assert_eq!(
            store.run_facts().unwrap().summary.exhausted(Phase::Review),
            1
        );
        rely(&actor);
        assert_eq!(queued(&store, Phase::Review), [NodeId::from(id)]);

        verdict(&store, id, Verdict::Validated);
        for _ in 0..2 {
            let (prover, job, _) = begun(&store, &Begin::Test(vec![id]));
            test(&store, &prover, id, TestResult::Pending);
            end_job(&store, job);
        }
        assert_eq!(queued(&store, Phase::Test), [NodeId::from(id)]);
    }

    /// Every phase's statements, the views deriving standing and the open tests seek the log,
    /// the task log and the graph by node: the only scans are of a per-subject result (among
    /// them the premises a premise rests on or that rest on it, walked by index), the bound
    /// lists, the run's one row, the environments prepared for any candidate, the paths for any
    /// sweep a chain waits on, the facts selecting tests, the root groups they are read for and
    /// the live jobs, uncorrelated and so read once per statement. Only a phase's whole listing
    /// reads every group's facts, a page seeks the range of subjects between its cursor and its
    /// window's end, and the subjects of live tasks and the spent work are read once, as lists
    /// each subject is looked up in. No task is sought by its phase alone, which would visit the phase's whole
    /// task log, and a node's events are walked only down its effects, which hold none of its
    /// test records. Coverage reads its root groups' facts once: no candidate's standing seeks
    /// its group's members.
    #[test]
    fn phase_statements_seek_by_node() {
        let (_session, store, _) = fixture();
        let mut statements = vec![
            "SELECT * FROM candidate_state WHERE candidate=?1",
            "SELECT * FROM finding_scenario WHERE root=?1",
            crate::store::tasks::OF_NODE,
            crate::store::synthesis::OPEN_TESTS,
        ];
        let (mut whole, mut pages) = (Vec::new(), Vec::new());
        for phase in Phase::ALL {
            let sql = sql(phase);
            whole.push(sql.owed);
            pages.push(sql.page);
            statements.extend([sql.page, crate::store::bearing::lookup(phase)]);
            if phase.bundled() {
                statements.extend([sql.naming, sql.bundle]);
            }
        }
        let every_group = "SCAN n USING INDEX node_live";
        let allowed = [
            "SCAN ob",
            "SCAN cp",
            "SCAN vp",
            "SCAN tp",
            "SCAN json_each VIRTUAL TABLE",
            "SCAN run",
            "SCAN rp",
            "SCAN CONSTANT ROW",
            "SCAN ev USING INDEX environment_unscoped",
            "SCAN tl",
            "SCAN upheld",
            "SCAN validated",
            "SCAN ur",
            "SCAN (subquery-",
            "SCAN j USING INDEX job_status_live",
            "SCAN above",
            "SCAN below",
        ];
        let statements =
            (statements.iter().map(|sql| (sql, false))).chain(whole.iter().map(|sql| (sql, true)));
        let mut scans: Vec<String> = statements
            .flat_map(|(sql, whole)| {
                (plan(&store, sql).into_iter())
                    .filter(move |Step { detail, within }| {
                        let listed = [
                            "SCAN lt",
                            "SEARCH lt ",
                            "SCAN sw",
                            "SEARCH sw ",
                            "BLOOM FILTER ON sw ",
                        ];
                        if listed.iter().any(|list| detail.starts_with(list)) {
                            return !within
                                .as_ref()
                                .is_some_and(|within| within.starts_with("LIST SUBQUERY"));
                        }
                        (detail.starts_with("SCAN ")
                            && !(whole && detail.starts_with(every_group))
                            && !allowed.iter().any(|scan| detail.starts_with(scan)))
                            || detail.ends_with("(phase=?)")
                            || (["SEARCH en ", "SEARCH ed "].iter())
                                .any(|walk| detail.starts_with(walk))
                                && !detail.contains("event_node_effect")
                    })
                    .map(move |step| format!("{} in {sql}", step.detail))
            })
            .collect();
        let range = "SEARCH n USING INDEX node_live (kind=? AND id>? AND id<?)";
        scans.extend(
            (pages.iter())
                .filter(|sql| !plan(&store, sql).iter().any(|step| step.detail == range))
                .map(|sql| format!("no range seek in {sql}")),
        );
        let coverage = crate::store::synthesis::CANDIDATE_STATES;
        scans.extend(
            (plan(&store, coverage).into_iter())
                .filter(|step| step.detail.contains("candidate_flaw_flaw"))
                .map(|step| format!("{} in {coverage}", step.detail)),
        );
        assert!(scans.is_empty(), "{scans:#?}");
    }
}

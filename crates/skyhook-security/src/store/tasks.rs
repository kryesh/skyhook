//! Tasks: the log of work an agent's job began in one phase, over one or more subjects, each
//! at the basis its phase's work rested on then: the latest event of the phase's `bearing` on
//! it. A task records nothing about how the work ended: the work is done once its subject no
//! longer owes the phase, and the task is live while its job is.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use libsql::Row;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook::{
    db::{Db, enum_column, params},
    identity::JobId,
    job::JobState,
};

use super::*;

/// Work to begin: one phase and the subjects it takes, typed by phase.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(
    tag = "phase",
    content = "subjects",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Begin {
    Sweep(Vec<PathId>),
    Probe(Vec<PathId>),
    Check(Vec<StateId>),
    Settle(Vec<PremiseId>),
    Chain(Vec<FlawId>),
    Review(Vec<CandidateId>),
    Test(Vec<CandidateId>),
    /// Root flaws.
    Report(Vec<FlawId>),
}

impl Begin {
    pub fn phase(&self) -> Phase {
        match self {
            Self::Sweep(_) => Phase::Sweep,
            Self::Probe(_) => Phase::Probe,
            Self::Check(_) => Phase::Check,
            Self::Settle(_) => Phase::Settle,
            Self::Chain(_) => Phase::Chain,
            Self::Review(_) => Phase::Review,
            Self::Test(_) => Phase::Test,
            Self::Report(_) => Phase::Report,
        }
    }

    fn subjects(&self) -> Vec<NodeId> {
        fn nodes<I: Into<NodeId> + Copy>(ids: &[I]) -> Vec<NodeId> {
            ids.iter().map(|id| (*id).into()).collect()
        }
        match self {
            Self::Sweep(ids) | Self::Probe(ids) => nodes(ids),
            Self::Check(ids) => nodes(ids),
            Self::Settle(ids) => nodes(ids),
            Self::Chain(ids) | Self::Report(ids) => nodes(ids),
            Self::Review(ids) | Self::Test(ids) => nodes(ids),
        }
    }
}

/// The live node `id` names, of `phase`'s subject kind, with what `phase`'s work on it rests
/// on now.
fn basis(db: &Db, phase: Phase, id: NodeId) -> Result<(NodeId, EventId)> {
    let (node, kind, basis) = optional(db, bearing::lookup(phase), params![id], |row| {
        Ok((
            NodeId(row.get(0)?),
            enum_column::<NodeKind>(row, 1)?,
            EventId(row.get(2)?),
        ))
    })?
    .ok_or(Error::NotFound {
        record: Record::Node(Some(phase.subject())),
        id: Some(id.0),
    })?;
    if kind != phase.subject() {
        return Err(Error::WrongKind {
            id,
            expected: phase.subject(),
            found: kind,
        });
    }
    Ok((node, basis))
}

/// One begun task as the log shows it: its phase and subjects, and its job's state and times.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Task {
    pub id: TaskId,
    pub phase: Phase,
    pub subjects: Vec<NodeId>,
    pub state: JobState,
    pub began: DateTime<Utc>,
    pub ended: Option<DateTime<Utc>>,
}

impl Keyed for Task {
    fn key(&self) -> i64 {
        self.id.0
    }
}

/// Which part of the task log a listing reads: tasks whose job is live, or those whose job
/// ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liveness {
    Live,
    Ended,
}

/// A listed task with the brief its job was launched with.
#[derive(Clone, Debug)]
pub struct ListedTask {
    pub task: Task,
    pub brief: Option<String>,
}

/// A task with its brief, its child's reply, and what its child recorded.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct TaskDetail {
    #[serde(flatten)]
    pub task: Task,
    pub job: JobId,
    /// The prompt the task's job was launched with.
    pub brief: Option<String>,
    /// Its child's latest reply; none until it replies.
    pub reply: Option<String>,
    /// The first events its child's agent recorded, oldest first, as many as asked for.
    pub events: Vec<Event>,
    /// Its child recorded more events than these.
    pub more: bool,
}

/// Tasks read as `t`, decoded by [`task_row`] without their subjects, then their jobs; a
/// condition follows.
macro_rules! task_select {
    () => {
        "SELECT t.id,j.state,e.at,j.ended_millis,t.phase,t.job FROM task t JOIN job_status j ON \
         j.job=t.job JOIN event e ON e.id=t.began WHERE "
    };
}

fn task_row(row: &Row) -> Result<Task> {
    Ok(Task {
        id: TaskId(row.get(0)?),
        phase: enum_column(row, 4)?,
        subjects: Vec::new(),
        state: enum_column(row, 1)?,
        began: time(row.get(2)?)?,
        ended: row.get::<Option<i64>>(3)?.map(time).transpose()?,
    })
}

/// Each subject of a live task, a merged one as the node it was merged into, as `task`, `phase`,
/// `node` and `began` rows: driven by the journal's live jobs, read down their index, never by the
/// task log's history, whatever statistics the planner lacks. The node keeps an integer's
/// affinity, so that a relation of these rows is sought by phase and node.
macro_rules! live {
    () => {
        "SELECT t.id AS task,tn.phase,CAST(coalesce(n.merged_into,n.id) AS INTEGER) AS node,\
         t.began FROM job_status j CROSS JOIN task t ON t.job=j.job JOIN task_node tn ON \
         tn.task=t.id JOIN node n ON n.id=tn.node WHERE NOT j.terminal"
    };
}
pub(super) use live;

/// The latest `$limit` tasks of node `$n`, or of a node merged into it, newest first, as `task`
/// rows: the newest of its own and of those merged into it, each read down its index, a task
/// on several merged nodes counted once.
macro_rules! newest_tasks {
    ($n:expr, $limit:expr) => {
        concat!(
            "SELECT task FROM (SELECT task FROM task_node WHERE node=",
            $n,
            " ORDER BY task DESC LIMIT ",
            $limit,
            ") UNION SELECT task FROM (SELECT DISTINCT tn.task FROM node m JOIN task_node tn ON \
             tn.node=m.id WHERE m.merged_into=",
            $n,
            " ORDER BY tn.task DESC LIMIT ",
            $limit,
            ") ORDER BY task DESC LIMIT ",
            $limit
        )
    };
}
pub(super) use newest_tasks;

/// The latest five tasks of node `?1`, or of a node merged into it, newest first: as many as a
/// node view names.
pub(super) const OF_NODE: &str = newest_tasks!("?1", "5");

/// The latest tasks of node `node`, or of a node merged into it, newest first.
pub(super) fn of_node(db: &Db, node: NodeId) -> Result<Vec<TaskId>> {
    rows(db, OF_NODE, params![node], |row| Ok(TaskId(row.get(0)?)))
}

/// `tasks` with their subjects.
fn with_subjects(db: &Db, mut tasks: Vec<Task>) -> Result<Vec<Task>> {
    let ids: Vec<i64> = tasks.iter().map(|task| task.id.0).collect();
    let mut subjects = BTreeMap::<i64, Vec<NodeId>>::new();
    for (task, node) in rows(
        db,
        "SELECT tn.task,tn.node FROM json_each(?1) j JOIN task_node tn ON tn.task=j.value ORDER \
         BY tn.task,tn.node",
        params![json_list(&ids)?],
        |row| Ok((row.get::<i64>(0)?, NodeId(row.get(1)?))),
    )? {
        subjects.entry(task).or_default().push(node);
    }
    for task in &mut tasks {
        task.subjects = subjects.remove(&task.id.0).unwrap_or_default();
    }
    Ok(tasks)
}

/// A journal job id as the store holds it.
pub(super) fn journal_job(id: i64) -> Result<JobId> {
    JobId::new(id as u64).map_err(|_| corrupt("invalid job id").into())
}

/// A begun task, with the run's scope, what changed of its subjects' work since each was last
/// worked in its phase, and the results proposed for it to decide.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct Begun {
    pub task: TaskId,
    pub scope: Scope,
    /// For each subject a task of this phase worked before, the events bearing on its work
    /// since that task's basis; nothing on a first visit.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<SubjectChanges>,
    /// Each subject's result recorded outside this phase's work that differs from its standing
    /// one, which this work decides.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub proposals: Vec<SubjectProposal>,
    /// For each state a check begins on, the decided premises of the flaws leaving it wrong,
    /// which what the check finds may contradict.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub premises: Vec<SubjectPremises>,
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct SubjectPremises {
    pub node: NodeId,
    pub premises: Vec<DecidedPremise>,
}

/// A subject's proposed result.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct SubjectProposal {
    pub node: NodeId,
    pub proposal: Proposal<Proposed>,
}

impl Store {
    /// Begin `work` as the task of `actor`'s job, each subject at the basis its phase's work
    /// rests on now, whether or not it is owed, with what changed for each since the basis of
    /// its previous task in the phase, on it or on a node merged into it. A merged subject is
    /// begun as the node it was merged into.
    pub fn begin(&self, actor: &AgentId, work: &Begin) -> Result<Begun> {
        let subjects = work.subjects();
        if subjects.is_empty() {
            return Err(Error::NoSubjects);
        }
        if subjects.len() > MAX_ATTACHMENTS {
            return Err(Error::Limit(Bound::Attachments));
        }
        let phase = work.phase();
        let session = self.id();
        self.write(|db| {
            let mut bases: Vec<(NodeId, EventId)> = Vec::with_capacity(subjects.len());
            for id in subjects {
                let (node, basis) = basis(db, phase, id)?;
                if !bases.iter().any(|(seen, _)| *seen == node) {
                    bases.push((node, basis));
                }
            }
            let job = optional(
                db,
                "SELECT owner_job FROM agent WHERE id=?1 AND owner_job IS NOT NULL",
                params![agent_row(db, actor)?],
                |row| Ok(row.get::<i64>(0)?),
            )?
            .ok_or(Error::TaskNeedsJob)?;
            let (mut changed, mut proposals, mut premises) = (Vec::new(), Vec::new(), Vec::new());
            for (node, _) in &bases {
                if phase == Phase::Check {
                    let decided = reads::decided_premises(db, *node)?;
                    if !decided.is_empty() {
                        premises.push(SubjectPremises {
                            node: *node,
                            premises: decided,
                        });
                    }
                }
                for proposal in reads::proposed(db, session, phase, *node)? {
                    proposals.push(SubjectProposal {
                        node: *node,
                        proposal,
                    });
                }
                let previous = stored(
                    db,
                    "SELECT max((SELECT max(tn.basis) FROM task_node tn WHERE tn.phase=?1 AND \
                     tn.node=m.id)) FROM node m WHERE m.id=?2 OR m.merged_into=?2",
                    params![phase, *node],
                    |row| Ok(row.get::<Option<i64>>(0)?.map(EventId)),
                )?;
                if let Some(previous) = previous {
                    let events = changes::noticed(db, phase, *node, previous)?;
                    let events = events.into_iter().collect();
                    changed.extend(changes::digest(db, phase, *node, &events)?);
                }
            }
            audit(db, actor, EventKind::Began, None, |event| {
                let task = TaskId(db.insert(
                    "INSERT INTO task(job,phase,began) VALUES (?1,?2,?3)",
                    params![job, phase, event],
                )?);
                let mut marks = Marks::default();
                for (node, basis) in bases {
                    db.execute(
                        "INSERT INTO task_node(task,phase,node,kind,basis) VALUES (?1,?2,?3,?4,?5)",
                        params![task, phase, node, phase.subject(), basis],
                    )?;
                    if phase == Phase::Report {
                        bearing::record_scenarios(db, event, node)?;
                    }
                    marks.mark(node, EventEffect::Concerns);
                }
                Ok((
                    Begun {
                        task,
                        scope: scope::scope(db)?,
                        changes: changed,
                        proposals,
                        premises,
                    },
                    marks,
                ))
            })
        })
    }

    /// The newest `limit` tasks of `liveness`, each with the brief its job was launched with.
    pub fn tasks(&self, liveness: Liveness, limit: usize) -> Result<Vec<ListedTask>> {
        let listed = self.read(|db| {
            let sql = match liveness {
                Liveness::Live => concat!(task_select!(), "NOT j.terminal ORDER BY t.id DESC"),
                Liveness::Ended => concat!(task_select!(), "j.terminal ORDER BY t.id DESC"),
            };
            let row = |row: &Row| Ok((task_row(row)?, row.get::<i64>(5)?));
            let (tasks, jobs): (Vec<_>, Vec<_>) = first_rows(db, sql, Vec::new(), limit, row)?
                .into_iter()
                .unzip();
            Ok(with_subjects(db, tasks)?.into_iter().zip(jobs))
        })?;
        let brief = |job| Ok::<_, Error>(self.0.agent_job_brief(journal_job(job)?)?);
        (listed.into_iter())
            .map(|(task, job)| {
                Ok(ListedTask {
                    task,
                    brief: brief(job)?,
                })
            })
            .collect()
    }

    /// A page of tasks, oldest first.
    pub fn task_page(&self, page: Page) -> Result<Listing<Task>> {
        self.read(|db| {
            let tasks = first_rows(
                db,
                concat!(task_select!(), "t.id>?1 ORDER BY t.id"),
                params![page.after],
                page.limit(),
                task_row,
            )?;
            Ok(Listing::keyset(with_subjects(db, tasks)?, page))
        })
    }

    pub fn task_count(&self, liveness: Liveness) -> Result<u64> {
        let sql = match liveness {
            Liveness::Live => {
                "SELECT count(*) FROM task t JOIN job_status j ON j.job=t.job WHERE NOT j.terminal"
            }
            Liveness::Ended => {
                "SELECT count(*) FROM task t JOIN job_status j ON j.job=t.job WHERE j.terminal"
            }
        };
        self.read(|db| count(db, sql, Vec::new()))
    }

    /// The task with its brief, its child's reply, and the first `limit` events its child
    /// recorded.
    pub fn task(&self, id: TaskId, limit: usize) -> Result<TaskDetail> {
        let session = self.id();
        let (task, job, mut events) = self.read(|db| {
            let (task, job) =
                optional(db, concat!(task_select!(), "t.id=?1"), params![id], |row| {
                    Ok((task_row(row)?, row.get::<i64>(5)?))
                })?
                .ok_or(Error::NotFound {
                    record: Record::Task,
                    id: Some(id.0),
                })?;
            let task = with_subjects(db, vec![task])?.remove(0);
            let events = events::of_job(db, session, job, limit.saturating_add(1))?;
            Ok((task, job, events))
        })?;
        let more = events.len() > limit;
        events.truncate(limit);
        let job = journal_job(job)?;
        let texts = self.0.agent_job_texts(job)?;
        Ok(TaskDetail {
            task,
            job,
            brief: texts.as_ref().map(|texts| texts.prompt.clone()),
            reply: texts.and_then(|texts| texts.reply),
            events,
            more,
        })
    }

    /// The journal's latest job transition, a finish included: job state the research
    /// generation does not follow moves it, so a cache of what reads task liveness keys on it.
    /// Any job's transition moves it, read at the end of each table's key.
    pub fn journal_mark(&self) -> Result<Option<i64>> {
        self.read(|db| {
            stored(
                db,
                "SELECT max(entry) FROM (SELECT max(entry) AS entry FROM job_transition UNION ALL \
                 SELECT max(entry) FROM job_finish)",
                Vec::new(),
                |row| Ok(row.get::<Option<i64>>(0)?),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use skyhook::job::JobState;

    use super::{
        super::tests::{
            HIGH, NONE, assess, begun, child_agent, end_job, fixture, last_event, premise,
            scenario, state, text, verdict, within,
        },
        *,
    };

    /// `begin` logs each subject at the latest event its phase reads, a merged one as the node
    /// it was merged into, and the log lists the task with its job's state, brief and what its
    /// child recorded. A subject of another kind, or an actor without a job, begins nothing.
    #[test]
    fn begin_logs_subjects_at_their_phases_bases() {
        let (_session, store, actor) = fixture();
        let (first, second) = (scenario(&store, &actor, "a"), scenario(&store, &actor, "b"));
        let created = last_event(&store);
        assess(&store, second.candidate, HIGH);
        let merge = Merge {
            node: first.candidate.into(),
            into: second.candidate.into(),
            note: text("same"),
        };
        store.merge(&actor, &merge).unwrap();
        let merged = last_event(&store);
        assert_matches!(
            store.begin(&actor, &Begin::Review(vec![second.candidate])),
            Err(Error::TaskNeedsJob)
        );
        let wrong = Begin::Review(vec![CandidateId(second.flaw.0)]);
        let (child, _) = child_agent(&store, JobState::Running);
        assert_matches!(
            store.begin(&child, &wrong),
            Err(Error::WrongKind {
                expected: NodeKind::Candidate,
                found: NodeKind::Flaw,
                ..
            })
        );
        let work = Begin::Review(vec![first.candidate, second.candidate]);
        let (_, _, task) = begun(&store, &work);
        let bases = store
            .read(|db| {
                rows(
                    db,
                    "SELECT node,basis FROM task_node WHERE task=?1",
                    params![task],
                    |row| Ok((NodeId(row.get(0)?), EventId(row.get(1)?))),
                )
            })
            .unwrap();
        assert_eq!(bases, [(second.candidate.into(), merged)]);
        assert!(created < merged);

        let tasks = store.tasks(Liveness::Live, 10).unwrap();
        assert_eq!(tasks[0].brief.as_deref(), Some("brief"));
        let task0 = &tasks[0].task;
        assert_eq!(
            (task0.id, task0.phase, &task0.subjects[..], task0.state),
            (
                task,
                Phase::Review,
                &[NodeId::from(second.candidate)][..],
                JobState::Running
            )
        );
        let detail = store.task(task, 10).unwrap();
        assert_eq!(detail.brief.as_deref(), Some("brief"));
        assert_eq!(
            detail
                .events
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            [EventKind::Began]
        );
        assert!(!detail.more && store.task(task, 0).unwrap().more);
    }

    /// `begin` names, per subject, what bore on its work since its previous task's basis in the
    /// phase, with each event's note, but nothing for a subject on its first visit: a reopened
    /// settle learns what it rests on failed.
    #[test]
    fn begin_reports_what_changed_since_the_last_visit() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        let [parent, sub] = [(); 2].map(|_| premise(&store, &actor, PremiseScope::Code));
        let links = [
            Link::FlawPremise {
                flaw: scenario.flaw,
                premise: parent,
            },
            Link::PremiseBasis {
                premise: parent,
                rests_on: sub,
            },
        ];
        store.link(&actor, &links).unwrap();
        let (settler, job) = child_agent(&store, JobState::Running);
        let first = store.begin(&settler, &Begin::Settle(vec![parent])).unwrap();
        assert!(first.changes.is_empty(), "a first visit");
        let holds = StatusChange::Premise(parent, PremiseStatus::Holds);
        store.set_status(&settler, holds, &text("drained")).unwrap();
        end_job(&store, job);
        within(&store, &Begin::Settle(vec![sub]), |settler| {
            let fails = StatusChange::Premise(sub, PremiseStatus::Fails);
            store
                .set_status(settler, fails, &text("one exit keeps input"))
                .unwrap();
        });
        let failed = last_event(&store);

        let (settler, _) = child_agent(&store, JobState::Running);
        let again = store
            .begin(&settler, &Begin::Settle(vec![parent, sub]))
            .unwrap();
        assert_eq!(
            (again.changes.iter())
                .map(|subject| {
                    let notes = (subject.recent.iter())
                        .map(|notice| (notice.id, notice.note.as_deref()))
                        .collect();
                    (subject.node, notes)
                })
                .collect::<Vec<(NodeId, Vec<_>)>>(),
            [(parent.into(), vec![(failed, Some("one exit keeps input"))])]
        );
    }

    /// A check begins with the decided premises of the flaws leaving its state wrong, each with
    /// the settle task deciding it and its evidence, as the state's view lists them; an
    /// undecided one is not listed.
    #[test]
    fn a_check_begins_with_the_decided_premises_it_may_contradict() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        let window = state(&store, &actor, NewState::Instance);
        let [decided, open] = [(); 2].map(|_| premise(&store, &actor, PremiseScope::Code));
        let mut links = vec![Link::FlawState {
            flaw: scenario.flaw,
            state: window,
        }];
        links.extend([decided, open].map(|premise| Link::FlawPremise {
            flaw: scenario.flaw,
            premise,
        }));
        store.link(&actor, &links).unwrap();
        let settle = within(&store, &Begin::Settle(vec![decided]), |settler| {
            let holds = StatusChange::Premise(decided, PremiseStatus::Holds);
            store.set_status(settler, holds, &text("traced")).unwrap();
            store.tasks(Liveness::Live, 1).unwrap()[0].task.id
        });
        let support = NewEvidence {
            location: None,
            environment: None,
            stance: Stance::Supports,
            note: text("every caller checks it"),
        };
        store.evidence(&actor, decided.into(), &[support]).unwrap();
        let (checker, _) = child_agent(&store, JobState::Running);
        let begun = store.begin(&checker, &Begin::Check(vec![window])).unwrap();
        let [listed] = &begun.premises[..] else {
            panic!("one state's premises")
        };
        let [premise] = &listed.premises[..] else {
            panic!("the decided premise alone")
        };
        assert_eq!(
            (premise.premise.id, premise.premise.status, premise.task),
            (decided, PremiseStatus::Holds, Some(settle))
        );
        assert_eq!(premise.evidence, 1);
        let NodeView::State(view) = store.node(window.into()).unwrap() else {
            panic!("a state")
        };
        assert_eq!(view.premises, listed.premises);
    }

    /// A subject's previous visit counts a task on a node merged into it: the survivor's first
    /// task learns what changed since, its own creation and the merge.
    #[test]
    fn begin_reports_what_changed_since_a_merged_nodes_visit() {
        let (_session, store, actor) = fixture();
        let merged = premise(&store, &actor, PremiseScope::Code);
        begun(&store, &Begin::Settle(vec![merged]));
        let kept = premise(&store, &actor, PremiseScope::Code);
        let created = last_event(&store);
        let merge = Merge {
            node: merged.into(),
            into: kept.into(),
            note: text("same"),
        };
        store.merge(&actor, &merge).unwrap();
        let folded = last_event(&store);
        let (settler, _) = child_agent(&store, JobState::Running);
        let again = store.begin(&settler, &Begin::Settle(vec![kept])).unwrap();
        assert_eq!(
            (again.changes.iter())
                .map(|subject| {
                    let events = subject.recent.iter().map(|notice| notice.id).collect();
                    (subject.node, events)
                })
                .collect::<Vec<(NodeId, Vec<EventId>)>>(),
            [(kept.into(), vec![created, folded])]
        );
    }

    /// A node's latest tasks count a task once however many of the nodes merged into it it
    /// began on. Its view lists them newest first, and a node merged into it lists only its own.
    #[test]
    fn a_nodes_latest_tasks_count_each_task_once() {
        let (_session, store, actor) = fixture();
        let [a, b, c] = [(); 3].map(|_| premise(&store, &actor, PremiseScope::Code));
        let mut tasks: Vec<TaskId> = (0..4)
            .map(|_| begun(&store, &Begin::Settle(vec![a])).2)
            .collect();
        tasks.push(begun(&store, &Begin::Settle(vec![a, b])).2);
        for premise in [a, b] {
            let merge = Merge {
                node: premise.into(),
                into: c.into(),
                note: text("same"),
            };
            store.merge(&actor, &merge).unwrap();
        }
        tasks.reverse();
        assert_eq!(store.read(|db| of_node(db, c.into())).unwrap(), tasks);
        let listed = |id: PremiseId| store.node(id.into()).unwrap().head().tasks.clone();
        assert_eq!(listed(c), tasks);
        assert_eq!(listed(b), [tasks[0]]);
    }

    /// A review begins with every result recorded outside reviews that it decides: an
    /// assessment and a verdict proposed together are both listed.
    #[test]
    fn a_review_begins_with_its_assessment_and_verdict_proposals() {
        let (_session, store, actor) = fixture();
        let id = scenario(&store, &actor, "decoder").candidate;
        assess(&store, id, HIGH);
        verdict(&store, id, Verdict::Validated);
        (store.assess(&actor, id, NONE.parse().unwrap(), &text("no impact"))).unwrap();
        (store.verdict(&actor, id, Verdict::Disproved, &text("refuted"))).unwrap();
        let (reviewer, _) = child_agent(&store, JobState::Running);
        let begun = store.begin(&reviewer, &Begin::Review(vec![id])).unwrap();
        assert_matches!(
            (begun.proposals.iter())
                .map(|subject| (subject.node, &subject.proposal.value))
                .collect::<Vec<_>>()[..],
            [
                (a, Proposed::Assessment(Scored { vector, .. })),
                (b, Proposed::Verdict(Verdict::Disproved)),
            ] if a == id.into() && b == id.into() && vector == NONE
        );
    }
}

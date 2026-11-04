//! The synthesis of the run's work: what every phase owes, what runs, what two tasks tried
//! without settling, what the user is asked, what stands unproven, what no path reaches,
//! which interfaces wait on an environment and what lies outside the scope; and the counts the
//! Research page draws.

use std::{collections::BTreeMap, sync::Arc};

use schemars::JsonSchema;
use serde::Serialize;
use skyhook::{
    db::{Db, enum_column, params},
    identity::JobId,
    named_enum::NamedEnum,
};

use super::*;

/// A subject two tasks tried at its current basis, or the scheduler spent there, none live,
/// that still owes the phase.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Exhausted {
    pub phase: Phase,
    pub node: NodeId,
    pub title: String,
    /// The tasks that tried it at its current basis, oldest first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<TaskId>,
    /// The last task's child's reply, when a task of it began.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply: Option<String>,
}

/// A deployment premise for the user to answer: undecided while a review waits on it, or
/// carrying a status proposed within scheduled work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Asked {
    pub premise: PremiseId,
    pub title: String,
}

/// A candidate whose unproven verdict stands, with the undecided premises of its chain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Unproven {
    pub candidate: CandidateId,
    pub title: String,
    pub premises: Vec<PremiseRef>,
}

/// An interface that unswept paths reading no caller-supplied state in code wait on: no environment for any
/// candidate serves it, or the launch does no live work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Uncovered {
    pub interface: InterfaceName,
    /// How many paths wait on it.
    pub paths: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Synthesis {
    /// Per phase, the subjects owing it that may still start: no task live, not exhausted.
    pub owed: BTreeMap<Phase, u64>,
    /// Per phase, the subjects of live tasks.
    pub running: BTreeMap<Phase, u64>,
    pub exhausted: Vec<Exhausted>,
    pub asked: Vec<Asked>,
    pub unproven: Vec<Unproven>,
    /// Components no path steps through.
    pub unreached: Vec<ComponentId>,
    pub uncovered: Vec<Uncovered>,
    pub outside: Outside,
}

/// The run's work in counts, read without the details the synthesis gives: what the
/// orchestrator steers by and the Research page draws.
#[derive(Clone, Debug)]
pub struct Summary {
    /// Per phase, the subjects owing it that may still start.
    pub owed: BTreeMap<Phase, u64>,
    /// Per phase, the subjects of live tasks.
    pub running: BTreeMap<Phase, u64>,
    /// Per phase, the subjects exhausted while they owe it.
    pub exhausted: BTreeMap<Phase, u64>,
    /// Deployment premises for the user to answer.
    pub asked: u64,
    /// Interfaces unswept paths wait on.
    pub uncovered: u64,
}

impl Summary {
    pub fn owed(&self, phase: Phase) -> u64 {
        self.owed.get(&phase).copied().unwrap_or_default()
    }

    pub fn running(&self, phase: Phase) -> u64 {
        self.running.get(&phase).copied().unwrap_or_default()
    }

    pub fn exhausted(&self, phase: Phase) -> u64 {
        self.exhausted.get(&phase).copied().unwrap_or_default()
    }

    /// Whether the run's work is done: nothing owed, running, exhausted, asked or uncovered.
    pub fn complete(&self) -> bool {
        (self.owed.values().chain(self.running.values()))
            .chain(self.exhausted.values())
            .all(|count| *count == 0)
            && self.asked == 0
            && self.uncovered == 0
    }

    /// The summary in one line, e.g. `owed: 3 sweep, 2 settle; 4 running; exhausted: 1 chain;
    /// asked: 2; uncovered: 1`.
    pub fn line(&self) -> String {
        let counts = |counts: &BTreeMap<Phase, u64>| -> Vec<String> {
            (counts.iter())
                .filter(|(_, count)| **count > 0)
                .map(|(phase, count)| format!("{count} {phase}"))
                .collect()
        };
        let owed = counts(&self.owed);
        let running: u64 = self.running.values().sum();
        let exhausted = counts(&self.exhausted);
        let mut parts = vec![match owed.is_empty() {
            true => "nothing owed".to_owned(),
            false => format!("owed: {}", owed.join(", ")),
        }];
        if running > 0 {
            parts.push(format!("{running} running"));
        }
        if !exhausted.is_empty() {
            parts.push(format!("exhausted: {}", exhausted.join(", ")));
        }
        if self.asked > 0 {
            parts.push(format!("asked: {}", self.asked));
        }
        if self.uncovered > 0 {
            parts.push(format!("uncovered: {}", self.uncovered));
        }
        parts.join("; ")
    }
}

/// What each phase owes: how many subjects may still start, and the exhausted ones.
struct OwedWork {
    offered: BTreeMap<Phase, u64>,
    exhausted: Vec<(Phase, queues::Owed)>,
}

fn owed_work(db: &Db, spent: &queues::Spent) -> Result<OwedWork> {
    let mut work = OwedWork {
        offered: BTreeMap::new(),
        exhausted: Vec::new(),
    };
    for phase in Phase::ALL {
        let mut offered = 0;
        for entry in queues::owed(db, phase, spent)? {
            match entry.exhausted {
                true => work.exhausted.push((phase, entry)),
                false => offered += 1,
            }
        }
        work.offered.insert(phase, offered);
    }
    Ok(work)
}

fn running(db: &Db) -> Result<BTreeMap<Phase, u64>> {
    counts(
        db,
        concat!(
            "SELECT phase,count(*) FROM (",
            tasks::live!(),
            ") GROUP BY phase"
        ),
    )
}

/// The deployment premises for the user to answer, as `p.node` and `n.title`: undecided while
/// a review waits on one, or carrying a proposal while a live candidate rests on it.
macro_rules! asked {
    () => {
        concat!(
            "SELECT p.node,n.title FROM premise_standing p JOIN node n ON n.id=p.node WHERE \
             p.scope='deployment' AND n.merged_into IS NULL AND EXISTS (SELECT 1 FROM (",
            bearing::premise_candidates!("p.node"),
            ") ap JOIN node an ON an.id=ap.candidate WHERE an.merged_into IS NULL AND \
             (p.proposed IS NOT NULL OR (p.status='undecided' AND ",
            queues::review_owed!("ap.candidate"),
            ")))"
        )
    };
}

/// The live recorded components no live path steps through but a retracted one, as `c.node`.
macro_rules! unreached {
    () => {
        "SELECT c.node FROM component c JOIN node n ON n.id=c.node WHERE n.merged_into IS NULL AND \
         c.status='recorded' AND NOT EXISTS (SELECT 1 FROM path_step s JOIN node sn ON \
         sn.id=s.path JOIN path sp ON sp.node=s.path WHERE s.component=c.node AND \
         sn.merged_into IS NULL AND sp.status!='retracted')"
    };
}

/// The interfaces unswept probed paths within the scope read recorded caller-supplied states on
/// and wait for, as `xi.interface`, with how many paths wait.
macro_rules! uncovered_interfaces {
    () => {
        concat!(
            "SELECT xi.interface,count(DISTINCT p.node) FROM path p JOIN node pn ON pn.id=p.node \
             JOIN path_state xs ON xs.path=p.node AND xs.access='read' JOIN state xo ON \
             xo.node=xs.state AND xo.scope IN ('input','handed') JOIN interface_location xi ON \
             xi.location=xs.location WHERE pn.merged_into IS NULL AND p.status='unswept' AND \
             xo.status='recorded' AND ",
            queues::probed!("p.node"),
            " AND NOT ",
            queues::served!("xi.interface"),
            " AND NOT ",
            scope::unscoped!("p.node"),
            " GROUP BY xi.interface"
        )
    };
}

fn summary(db: &Db) -> Result<Summary> {
    let work = owed_work(db, &queues::Spent::default())?;
    let mut exhausted = BTreeMap::new();
    for (phase, _) in work.exhausted {
        *exhausted.entry(phase).or_default() += 1;
    }
    Ok(Summary {
        owed: work.offered,
        running: running(db)?,
        exhausted,
        asked: count(
            db,
            concat!("SELECT count(*) FROM (", asked!(), ")"),
            Vec::new(),
        )?,
        uncovered: count(
            db,
            concat!("SELECT count(*) FROM (", uncovered_interfaces!(), ")"),
            Vec::new(),
        )?,
    })
}

/// A synthesis as one snapshot finds it, each exhausted subject with the tasks that tried it at
/// its basis and the last one's job: [`Store::synthesis`] reads the replies outside the snapshot.
struct Found {
    owed: BTreeMap<Phase, u64>,
    running: BTreeMap<Phase, u64>,
    exhausted: Vec<(Phase, queues::Owed, Vec<TaskId>, Option<JobId>)>,
    asked: Vec<Asked>,
    unproven: Vec<Unproven>,
    unreached: Vec<ComponentId>,
    uncovered: Vec<Uncovered>,
    outside: Outside,
}

fn synthesis(db: &Db, spent: &queues::Spent) -> Result<Found> {
    let work = owed_work(db, spent)?;
    let mut exhausted = Vec::with_capacity(work.exhausted.len());
    for (phase, entry) in work.exhausted {
        let tried = rows(
            db,
            "SELECT t.id,t.job FROM task_node tn JOIN task t ON t.id=tn.task WHERE tn.phase=?1 AND \
             tn.node=?2 AND tn.basis=?3 ORDER BY tn.task",
            params![phase, entry.node, entry.basis],
            |row| Ok((TaskId(row.get(0)?), tasks::journal_job(row.get(1)?)?)),
        )?;
        let job = tried.last().map(|(_, job)| *job);
        let tasks = tried.into_iter().map(|(task, _)| task).collect();
        exhausted.push((phase, entry, tasks, job));
    }
    let unproven = rows(
        db,
        "SELECT c.candidate,n.title FROM candidate_result c JOIN node n ON n.id=c.candidate \
         WHERE c.verdict='unproven' AND (SELECT state FROM candidate_state WHERE \
         candidate=c.candidate)='unproven' ORDER BY c.candidate",
        Vec::new(),
        |row| Ok((CandidateId(row.get(0)?), row.get::<String>(1)?)),
    )?
    .into_iter()
    .map(|(candidate, title)| {
        Ok(Unproven {
            premises: reads::undecided_premises(db, candidate.into())?,
            candidate,
            title,
        })
    })
    .collect::<Result<_>>()?;
    Ok(Found {
        owed: work.offered,
        unproven,
        running: running(db)?,
        exhausted,
        asked: rows(
            db,
            concat!(asked!(), " ORDER BY p.node"),
            Vec::new(),
            |row| {
                Ok(Asked {
                    premise: PremiseId(row.get(0)?),
                    title: row.get(1)?,
                })
            },
        )?,
        unreached: rows(
            db,
            concat!(unreached!(), " ORDER BY c.node"),
            Vec::new(),
            |row| Ok(ComponentId(row.get(0)?)),
        )?,
        uncovered: rows(
            db,
            concat!(uncovered_interfaces!(), " ORDER BY xi.interface"),
            Vec::new(),
            |row| {
                Ok(Uncovered {
                    interface: row.get::<String>(0)?.parse()?,
                    paths: row.get(1)?,
                })
            },
        )?,
        outside: scope::outside(db)?,
    })
}

/// Counts per value of the enum in the first column.
fn counts<T: NamedEnum + Ord>(db: &Db, sql: &'static str) -> Result<BTreeMap<T, u64>> {
    let pairs = rows(db, sql, Vec::new(), |row| {
        Ok((enum_column(row, 0)?, row.get::<u64>(1)?))
    })?;
    Ok(pairs.into_iter().collect())
}

/// What the run holds, by kind and standing, merged and retracted nodes aside.
#[derive(Clone, Debug, Default, Serialize, JsonSchema)]
pub struct Coverage {
    pub states: u64,
    pub paths: u64,
    pub swept: u64,
    pub components: u64,
    /// Components no path steps through.
    pub unreached: u64,
    /// Live premises by status.
    pub premises: BTreeMap<PremiseStatus, u64>,
    /// Live premises disputed: a proposal disagrees with their standing, or contradicting
    /// evidence came after it.
    pub disputed: u64,
    pub flaws: BTreeMap<FlawStatus, u64>,
    /// Live candidates by status.
    pub candidates: BTreeMap<Status, u64>,
    /// Current findings: those `generate` publishes.
    pub findings: u64,
    /// Tasks begun, by phase.
    pub tasks: BTreeMap<Phase, u64>,
    pub outside: Outside,
}

/// The live candidates by status, their root groups' facts read once.
pub(super) const CANDIDATE_STATES: &str = concat!(
    "WITH ",
    bearing::demonstrations!(),
    " SELECT ",
    bearing::state!("ifnull(d.rank>=s.rank,0)"),
    ",count(*) FROM candidate_core s LEFT JOIN demonstration d ON d.root=s.root WHERE \
     s.merged_into IS NULL GROUP BY 1"
);

fn coverage(db: &Db) -> Result<Coverage> {
    let (states, paths, swept, components) = stored(
        db,
        "SELECT (SELECT count(*) FROM state s JOIN node n ON n.id=s.node WHERE n.merged_into IS \
         NULL AND s.status='recorded'),(SELECT count(*) FROM path p JOIN node n ON n.id=p.node \
         WHERE n.merged_into IS NULL AND p.status!='retracted'),\
         (SELECT count(*) FROM path p JOIN node n ON n.id=p.node WHERE n.merged_into IS NULL AND \
         p.status='swept'),(SELECT count(*) FROM component c JOIN node n ON n.id=c.node WHERE \
         n.merged_into IS NULL AND c.status='recorded')",
        Vec::new(),
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    Ok(Coverage {
        states,
        paths,
        swept,
        components,
        unreached: count(
            db,
            concat!("SELECT count(*) FROM (", unreached!(), ")"),
            Vec::new(),
        )?,
        premises: counts(
            db,
            "SELECT p.status,count(*) FROM premise_standing p JOIN node n ON n.id=p.node WHERE \
             n.merged_into IS NULL GROUP BY p.status",
        )?,
        disputed: count(
            db,
            concat!(
                "SELECT count(*) FROM premise_standing p JOIN node n ON n.id=p.node WHERE \
                 n.merged_into IS NULL AND ",
                bearing::disputed!("p", "decided")
            ),
            Vec::new(),
        )?,
        flaws: counts(
            db,
            "SELECT f.status,count(*) FROM flaw_standing f JOIN node n ON n.id=f.node WHERE \
             n.merged_into IS NULL GROUP BY f.status",
        )?,
        candidates: counts(db, CANDIDATE_STATES)?,
        findings: count(
            db,
            concat!(
                "SELECT count(*) FROM finding f WHERE ",
                queues::current_finding!("f.node")
            ),
            Vec::new(),
        )?,
        tasks: counts(db, "SELECT phase,count(*) FROM task GROUP BY phase")?,
        outside: scope::outside(db)?,
    })
}

/// What the orchestrator steers the run by, read at once: the run and the summary of its
/// work.
#[derive(Clone)]
pub struct RunFacts {
    pub run: Run,
    pub summary: Summary,
    pub artifacts: std::path::PathBuf,
}

/// A candidate's latest test task, on it or on a candidate merged into it, which has recorded
/// no test since it began: live while it is proving it, abandoned once its job ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenTest {
    pub candidate: CandidateId,
    pub task: TaskId,
    pub live: bool,
}

/// Each live candidate's latest test task, read down its own and each merged candidate's tasks
/// newest first, while it recorded no test since; its job's state says whether it is live.
pub(super) const OPEN_TESTS: &str = concat!(
    "SELECT o.candidate,o.task,NOT j.terminal FROM (SELECT n.id AS candidate,(SELECT \
     max((SELECT tn.task FROM task_node tn WHERE tn.node=m.id AND +tn.phase='test' ORDER BY \
     tn.task DESC LIMIT 1)) FROM node m WHERE m.id=n.id OR m.merged_into=n.id) AS task FROM node \
     n WHERE n.kind='candidate' AND n.merged_into IS NULL) o JOIN task t ON t.id=o.task JOIN \
     job_status j ON j.job=t.job WHERE ",
    bearing::untested_since!("o.candidate", "t.began"),
    " ORDER BY o.candidate"
);

fn open_tests(db: &Db) -> Result<Vec<OpenTest>> {
    rows(db, OPEN_TESTS, Vec::new(), |row| {
        Ok(OpenTest {
            candidate: CandidateId(row.get(0)?),
            task: TaskId(row.get(1)?),
            live: row.get(2)?,
        })
    })
}

/// The run as the Research page shows it: its facts, what it holds and its open tests.
#[derive(Clone)]
pub struct RunState {
    pub facts: Arc<RunFacts>,
    pub coverage: Arc<Coverage>,
    pub open_tests: Vec<OpenTest>,
}

impl Store {
    /// What every phase owes, what runs, what is exhausted with its last child's reply, the
    /// scheduler's `spent` work included, what the user is asked, what stands unproven, what no
    /// path reaches and what lies outside the scope.
    pub fn synthesis(&self, spent: &[SpentWork]) -> Result<Synthesis> {
        let spent = &queues::Spent::of(spent)?;
        let found = self.read(|db| synthesis(db, spent))?;
        let exhausted = (found.exhausted.into_iter())
            .map(|(phase, owed, tasks, job)| {
                let texts = job.map(|job| self.0.agent_job_texts(job)).transpose()?;
                Ok(Exhausted {
                    reply: texts.flatten().and_then(|texts| texts.reply),
                    tasks,
                    phase,
                    node: owed.node,
                    title: owed.title,
                })
            })
            .collect::<Result<_>>()?;
        Ok(Synthesis {
            owed: found.owed,
            running: found.running,
            exhausted,
            asked: found.asked,
            unproven: found.unproven,
            unreached: found.unreached,
            uncovered: found.uncovered,
            outside: found.outside,
        })
    }

    pub fn coverage(&self) -> Result<Coverage> {
        self.read(coverage)
    }

    pub fn run_facts(&self) -> Result<RunFacts> {
        let (run, summary) = self.read(|db| Ok((runs::run(db)?, summary(db)?)))?;
        Ok(RunFacts {
            run,
            summary,
            artifacts: self.artifact_root(),
        })
    }

    pub fn open_tests(&self) -> Result<Vec<OpenTest>> {
        self.read(open_tests)
    }

    /// The run as the Research page shows it, read at once.
    pub fn run_state(&self) -> Result<RunState> {
        let (run, summary, coverage, open_tests) =
            self.read(|db| Ok((runs::run(db)?, summary(db)?, coverage(db)?, open_tests(db)?)))?;
        Ok(RunState {
            facts: Arc::new(RunFacts {
                run,
                summary,
                artifacts: self.artifact_root(),
            }),
            coverage: Arc::new(coverage),
            open_tests,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::{
            queues::tests::queued,
            tests::{
                HIGH, assess, begun, component, end_job, fixture, operation, path_from, premise,
                scenario, sweep, text, validated, verdict,
            },
        },
        *,
    };

    /// A deployment premise's status counts when recorded outside every task, as the user's
    /// answer relayed; recorded within scheduled work it only proposes one, which the user is
    /// asked until they answer.
    #[test]
    fn a_deployment_premise_is_answered_outside_scheduled_work() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        let deployment = premise(&store, &actor, PremiseScope::Deployment);
        let rests = Link::FlawPremise {
            flaw: scenario.flaw,
            premise: deployment,
        };
        store.link(&actor, &[rests]).unwrap();
        let answer = |agent: &AgentId, status| {
            let change = StatusChange::Premise(deployment, status);
            store.set_status(agent, change, &text("answered")).unwrap();
        };
        let read = || {
            let NodeView::Premise(view) = store.node(deployment.into()).unwrap() else {
                panic!("a premise")
            };
            let asked = store.synthesis(&[]).unwrap().asked;
            let asked: Vec<PremiseId> = asked.iter().map(|asked| asked.premise).collect();
            (view.status, view.proposed.map(|p| p.value), asked)
        };
        answer(&actor, PremiseStatus::Holds);
        assert_eq!(read(), (PremiseStatus::Holds, None, vec![]));
        let (reviewer, job, _) = begun(&store, &Begin::Review(vec![scenario.candidate]));
        answer(&reviewer, PremiseStatus::Fails);
        end_job(&store, job);
        assert_eq!(
            read(),
            (
                PremiseStatus::Holds,
                Some(PremiseStatus::Fails),
                vec![deployment]
            )
        );
        answer(&actor, PremiseStatus::Fails);
        assert_eq!(read(), (PremiseStatus::Fails, None, vec![]));
    }

    /// A candidate left unproven is listed with the undecided premises of its chain alone.
    #[test]
    fn an_unproven_candidate_is_listed_with_its_undecided_premises() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        sweep(&store, scenario.path);
        assess(&store, scenario.candidate, HIGH);
        let [open, decided] = [PremiseScope::Code, PremiseScope::Deployment]
            .map(|scope| premise(&store, &actor, scope));
        let rests = [open, decided].map(|premise| Link::FlawPremise {
            flaw: scenario.flaw,
            premise,
        });
        store.link(&actor, &rests).unwrap();
        let holds = StatusChange::Premise(decided, PremiseStatus::Holds);
        store
            .set_status(&actor, holds, &text("the user runs it"))
            .unwrap();
        verdict(&store, scenario.candidate, Verdict::Unproven);
        let unproven = store.synthesis(&[]).unwrap().unproven;
        assert_eq!(
            (unproven.iter())
                .map(|entry| (
                    entry.candidate,
                    entry.premises.iter().map(|p| p.id).collect()
                ))
                .collect::<Vec<(CandidateId, Vec<PremiseId>)>>(),
            [(scenario.candidate, vec![open])]
        );
    }

    /// A test task on a candidate merged away is its survivor's open test.
    #[test]
    fn a_merged_candidates_open_test_is_its_survivors() {
        let (_session, store, actor) = fixture();
        let (merged, kept) = (
            validated(&store, &actor, "a").candidate,
            validated(&store, &actor, "b").candidate,
        );
        let (_, _, task) = begun(&store, &Begin::Test(vec![merged]));
        let merge = Merge {
            node: merged.into(),
            into: kept.into(),
            note: text("same"),
        };
        store.merge(&actor, &merge).unwrap();
        assert_eq!(
            store.run_state().unwrap().open_tests,
            [OpenTest {
                candidate: kept,
                task,
                live: true
            }]
        );
    }

    /// A retracted input state is read in no path's work and counts nowhere: a path reading only
    /// it owes no sweep or probe, and an interface only it is read on waits on nothing. A
    /// retracted component is neither counted nor unreached. Restored, an input state is read
    /// on its paths again.
    #[test]
    fn retracted_states_and_components_count_nowhere() {
        let (_session, store, actor) = fixture();
        let decoder = scenario(&store, &actor, "decoder");
        let idle = component(&store, &actor, "idle");
        let gateway = component(&store, &actor, "gateway");
        let api = path_from(&store, &actor, "api", &[gateway], operation("gateway-api"));
        let [code, interface] = [decoder.path, api].map(|path| {
            let NodeView::Path(view) = store.node(path.into()).unwrap() else {
                panic!("a path")
            };
            view.states[0].id
        });
        let read = || {
            let (coverage, synthesis) = (store.coverage().unwrap(), store.synthesis(&[]).unwrap());
            let counts = (coverage.states, coverage.components, coverage.unreached);
            let uncovered = synthesis.uncovered.len();
            let structure = [Phase::Sweep, Phase::Probe].map(|phase| queued(&store, phase));
            (counts, synthesis.unreached, uncovered, structure.concat())
        };
        let swept = vec![NodeId::from(decoder.path)];
        assert_eq!(read(), ((2, 3, 1), vec![idle], 1, swept.clone()));
        let mark = |change| store.set_status(&actor, change, &text("in error")).unwrap();
        let retracted = StateMark::Inventory(InventoryStatus::Retracted);
        mark(StatusChange::State(code, retracted.clone()));
        mark(StatusChange::State(interface, retracted));
        mark(StatusChange::Component(idle, InventoryStatus::Retracted));
        assert_eq!(read(), ((0, 2, 0), vec![], 0, vec![]));
        let recorded = StateMark::Inventory(InventoryStatus::Recorded);
        mark(StatusChange::State(code, recorded));
        assert_eq!(read(), ((1, 2, 0), vec![], 0, swept));
    }
}

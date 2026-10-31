//! The synthesis of the run's work: what every phase owes, what runs, what two tasks tried
//! without settling, what the user is asked, and what no path reaches; and the counts the
//! Research page draws.

use std::collections::BTreeMap;

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
    /// The last task's child's reply, when a task of it began.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply: Option<String>,
}

/// An undecided deployment premise a review waits on: the user's to answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Asked {
    pub premise: PremiseId,
    pub title: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Synthesis {
    /// Per phase, the subjects owing it that may still start: no task live, not exhausted.
    pub owed: BTreeMap<Phase, u64>,
    /// Per phase, the subjects of live tasks.
    pub running: BTreeMap<Phase, u64>,
    pub exhausted: Vec<Exhausted>,
    pub asked: Vec<Asked>,
    /// Components no path steps through.
    pub unreached: Vec<ComponentId>,
}

impl Synthesis {
    pub fn owed(&self, phase: Phase) -> u64 {
        self.owed.get(&phase).copied().unwrap_or_default()
    }

    pub fn running(&self, phase: Phase) -> u64 {
        self.running.get(&phase).copied().unwrap_or_default()
    }

    pub fn exhausted(&self, phase: Phase) -> u64 {
        (self.exhausted.iter())
            .filter(|exhausted| exhausted.phase == phase)
            .count() as u64
    }

    /// Whether the run's work is done: nothing owed, running, exhausted or asked.
    pub fn complete(&self) -> bool {
        self.owed.values().all(|count| *count == 0)
            && self.running.values().all(|count| *count == 0)
            && self.exhausted.is_empty()
            && self.asked.is_empty()
    }

    /// The synthesis in one line, e.g. `owed: 3 sweep, 2 settle; 4 running; exhausted: 1
    /// chain; asked: 2`.
    pub fn line(&self) -> String {
        let counts = |counts: Vec<(Phase, u64)>| -> Vec<String> {
            (counts.into_iter())
                .filter(|(_, count)| *count > 0)
                .map(|(phase, count)| format!("{count} {phase}"))
                .collect()
        };
        let owed = counts(self.owed.iter().map(|(phase, n)| (*phase, *n)).collect());
        let running: u64 = self.running.values().sum();
        let exhausted = counts(
            (Phase::ALL.iter())
                .map(|phase| (*phase, self.exhausted(*phase)))
                .collect(),
        );
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
        if !self.asked.is_empty() {
            parts.push(format!("asked: {}", self.asked.len()));
        }
        parts.join("; ")
    }
}

/// A synthesis as one snapshot finds it, each exhausted subject with its last task's job if one
/// began: [`Store::replied`] reads the replies outside the snapshot.
struct Found {
    owed: BTreeMap<Phase, u64>,
    running: BTreeMap<Phase, u64>,
    exhausted: Vec<(Phase, queues::Owed, Option<JobId>)>,
    asked: Vec<Asked>,
    unreached: Vec<ComponentId>,
}

fn synthesis(db: &Db, spent: &queues::Spent) -> Result<Found> {
    let (mut owed, mut exhausted) = (BTreeMap::new(), Vec::new());
    for phase in Phase::ALL {
        let mut offered = 0;
        for entry in queues::owed(db, phase, spent)? {
            if !entry.exhausted {
                offered += 1;
                continue;
            }
            let job = optional(
                db,
                "SELECT t.job FROM task_node tn JOIN task t ON t.id=tn.task WHERE tn.phase=?1 AND \
                 tn.node=?2 AND tn.basis=?3 ORDER BY tn.task DESC LIMIT 1",
                params![phase, entry.node, entry.basis],
                |row| tasks::journal_job(row.get(0)?),
            )?;
            exhausted.push((phase, entry, job));
        }
        owed.insert(phase, offered);
    }
    Ok(Found {
        owed,
        running: counts(
            db,
            "SELECT tn.phase,count(*) FROM task_node tn JOIN task_state s ON s.task=tn.task WHERE \
             s.live GROUP BY tn.phase",
        )?,
        exhausted,
        asked: rows(
            db,
            concat!(
                "SELECT p.node,n.title FROM premise_state p JOIN node n ON n.id=p.node WHERE \
                 p.scope='deployment' AND p.status='undecided' AND n.merged_into IS NULL AND \
                 EXISTS (SELECT 1 FROM (",
                bearing::premise_candidates!("p.node"),
                ") ap JOIN node an ON an.id=ap.candidate WHERE an.merged_into IS NULL AND ",
                queues::review_owed!("ap.candidate"),
                ") ORDER BY p.node"
            ),
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
            "SELECT c.node FROM component c WHERE NOT EXISTS (SELECT 1 FROM path_step s WHERE \
             s.component=c.node) ORDER BY c.node",
            Vec::new(),
            |row| Ok(ComponentId(row.get(0)?)),
        )?,
    })
}

/// Counts per value of the enum in the first column.
fn counts<T: NamedEnum + Ord>(db: &Db, sql: &'static str) -> Result<BTreeMap<T, u64>> {
    let pairs = rows(db, sql, Vec::new(), |row| {
        Ok((enum_column(row, 0)?, row.get::<u64>(1)?))
    })?;
    Ok(pairs.into_iter().collect())
}

/// What the run holds, by kind and standing.
#[derive(Clone, Debug, Default, Serialize, JsonSchema)]
pub struct Coverage {
    pub sources: u64,
    pub paths: u64,
    pub swept: u64,
    pub components: u64,
    /// Live premises by status.
    pub premises: BTreeMap<PremiseStatus, u64>,
    /// Live premises with contradicting evidence since their latest status.
    pub disputed: u64,
    pub flaws: BTreeMap<FlawStatus, u64>,
    /// Live candidates by status.
    pub candidates: BTreeMap<Status, u64>,
    /// Current findings: those `generate` publishes.
    pub findings: u64,
    /// Tasks begun, by phase.
    pub tasks: BTreeMap<Phase, u64>,
}

fn coverage(db: &Db) -> Result<Coverage> {
    let all = |kind: NodeKind| count(db, "SELECT count(*) FROM node WHERE kind=?1", params![kind]);
    Ok(Coverage {
        sources: all(NodeKind::Source)?,
        paths: all(NodeKind::Path)?,
        swept: count(
            db,
            "SELECT count(*) FROM path WHERE swept IS NOT NULL",
            Vec::new(),
        )?,
        components: all(NodeKind::Component)?,
        premises: counts(
            db,
            "SELECT p.status,count(*) FROM premise_state p JOIN node n ON n.id=p.node WHERE \
             n.merged_into IS NULL GROUP BY p.status",
        )?,
        disputed: count(
            db,
            concat!(
                "SELECT count(*) FROM premise p JOIN node n ON n.id=p.node WHERE n.merged_into IS \
                 NULL AND ",
                reads::disputed!("p.node", "p.decided")
            ),
            Vec::new(),
        )?,
        flaws: counts(
            db,
            "SELECT f.status,count(*) FROM flaw_state f JOIN node n ON n.id=f.node WHERE \
             n.merged_into IS NULL GROUP BY f.status",
        )?,
        candidates: counts(
            db,
            "SELECT state,count(*) FROM candidate_state WHERE merged_into IS NULL GROUP BY state",
        )?,
        findings: count(
            db,
            concat!(
                "SELECT count(*) FROM finding f WHERE ",
                queues::current_finding!("f.node")
            ),
            Vec::new(),
        )?,
        tasks: counts(db, "SELECT phase,count(*) FROM task GROUP BY phase")?,
    })
}

/// What the orchestrator steers the run by, read at once: the run and the synthesis of its
/// work.
#[derive(Clone)]
pub struct RunFacts {
    pub run: Run,
    pub synthesis: Synthesis,
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

fn open_tests(db: &Db) -> Result<Vec<OpenTest>> {
    rows(
        db,
        "SELECT o.candidate,o.task,s.live FROM (SELECT coalesce(n.merged_into,n.id) AS \
         candidate,max(tn.task) AS task FROM task_node tn JOIN node n ON n.id=tn.node WHERE \
         tn.phase='test' GROUP BY 1) o JOIN task_state s ON s.task=o.task WHERE NOT EXISTS \
         (SELECT 1 FROM test x WHERE x.candidate=o.candidate AND x.event>s.began) ORDER BY \
         o.candidate",
        Vec::new(),
        |row| {
            Ok(OpenTest {
                candidate: CandidateId(row.get(0)?),
                task: TaskId(row.get(1)?),
                live: row.get(2)?,
            })
        },
    )
}

/// The run as the Research page shows it: its facts, what it holds and its open tests, read
/// at once.
#[derive(Clone)]
pub struct RunState {
    pub facts: RunFacts,
    pub coverage: Coverage,
    pub open_tests: Vec<OpenTest>,
}

impl Store {
    /// What every phase owes, what runs, what is exhausted with its last child's reply, the
    /// scheduler's `spent` work included, what the user is asked and what no path reaches.
    pub fn synthesis(&self, spent: &[SpentWork]) -> Result<Synthesis> {
        let spent = &queues::Spent::of(spent)?;
        let found = self.read(|db| synthesis(db, spent))?;
        self.replied(found)
    }

    /// The synthesis `found`, each exhausted subject with its last child's reply, read after
    /// the snapshot that found it.
    fn replied(&self, found: Found) -> Result<Synthesis> {
        let exhausted = (found.exhausted.into_iter())
            .map(|(phase, owed, job)| {
                let texts = job.map(|job| self.0.agent_job_texts(job)).transpose()?;
                Ok(Exhausted {
                    reply: texts.flatten().and_then(|texts| texts.reply),
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
            unreached: found.unreached,
        })
    }

    pub fn coverage(&self) -> Result<Coverage> {
        self.read(coverage)
    }

    pub fn run_facts(&self) -> Result<RunFacts> {
        let (run, found) =
            self.read(|db| Ok((runs::run(db)?, synthesis(db, &queues::Spent::default())?)))?;
        Ok(RunFacts {
            run,
            synthesis: self.replied(found)?,
            artifacts: self.artifact_root(),
        })
    }

    pub fn run_state(&self) -> Result<RunState> {
        let (run, found, coverage, open_tests) = self.read(|db| {
            Ok((
                runs::run(db)?,
                synthesis(db, &queues::Spent::default())?,
                coverage(db)?,
                open_tests(db)?,
            ))
        })?;
        let facts = RunFacts {
            run,
            synthesis: self.replied(found)?,
            artifacts: self.artifact_root(),
        };
        Ok(RunState {
            facts,
            coverage,
            open_tests,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::tests::{begun, fixture, text, validated},
        *,
    };

    /// A test task on a candidate merged away is its survivor's open test.
    #[test]
    fn a_merged_candidates_open_test_is_its_survivors() {
        let (_session, store, actor) = fixture();
        let (merged, kept) = (
            validated(&store, &actor, "a").candidate,
            validated(&store, &actor, "b").candidate,
        );
        let (_, _, task) = begun(&store, &Begin::Test(vec![merged]));
        let merge = Merge::Candidate {
            candidate: merged,
            into: kept,
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
}

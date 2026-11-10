//! The run as the orchestrator's host state and the Research page read it: its facts, what it
//! holds and its open tests.

use std::{collections::BTreeMap, sync::Arc};

use skyhook::db::Db;

use crate::store::work::synthesis::{counts, summary, unreached};
use crate::store::*;

/// The run's work in counts, read without the details the synthesis gives: what the
/// orchestrator steers by and the Research page draws.
#[derive(Clone, Debug)]
pub(crate) struct Summary {
    /// Per phase, the subjects owing it that may still start.
    pub(crate) owed: BTreeMap<Phase, u64>,
    /// Per phase, the subjects of live tasks.
    pub(crate) running: BTreeMap<Phase, u64>,
    /// Per phase, the subjects exhausted while they owe it.
    pub(crate) exhausted: BTreeMap<Phase, u64>,
    /// Deployment premises for the user to answer.
    pub(crate) asked: u64,
    /// Interfaces unswept paths wait on.
    pub(crate) uncovered: u64,
}

impl Summary {
    pub(crate) fn owed(&self, phase: Phase) -> u64 {
        self.owed.get(&phase).copied().unwrap_or_default()
    }

    pub(crate) fn running(&self, phase: Phase) -> u64 {
        self.running.get(&phase).copied().unwrap_or_default()
    }

    pub(crate) fn exhausted(&self, phase: Phase) -> u64 {
        self.exhausted.get(&phase).copied().unwrap_or_default()
    }

    /// Whether the run's work is done: nothing owed, running, exhausted, asked or uncovered.
    pub(crate) fn complete(&self) -> bool {
        (self.owed.values().chain(self.running.values()))
            .chain(self.exhausted.values())
            .all(|count| *count == 0)
            && self.asked == 0
            && self.uncovered == 0
    }

    /// The summary in one line, e.g. `owed: 3 sweep, 2 settle; 4 running; exhausted: 1 chain;
    /// asked: 2; uncovered: 1`.
    pub(crate) fn line(&self) -> String {
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

/// What the run holds, by kind and standing, merged and retracted nodes aside.
#[derive(Clone, Debug)]
pub(crate) struct Coverage {
    pub(crate) states: u64,
    pub(crate) paths: u64,
    pub(crate) swept: u64,
    pub(crate) components: u64,
    /// Components no path steps through.
    pub(crate) unreached: u64,
    /// Live premises by status.
    pub(crate) premises: BTreeMap<PremiseStatus, u64>,
    /// Live premises disputed: a proposal disagrees with their standing, or contradicting
    /// evidence came after it.
    pub(crate) disputed: u64,
    pub(crate) flaws: BTreeMap<FlawStatus, u64>,
    /// Live candidates by status.
    pub(crate) candidates: BTreeMap<Status, u64>,
    /// Current findings: those `generate` publishes.
    pub(crate) findings: u64,
    /// Tasks begun, by phase.
    pub(crate) tasks: BTreeMap<Phase, u64>,
    pub(crate) outside: Outside,
}

/// The live candidates by status, their root groups' facts read once.
const CANDIDATE_STATES: &str = concat!(
    "WITH ",
    crate::store::ledger::facts::demonstrations!(),
    " SELECT ",
    crate::store::ledger::facts::state!("ifnull(d.rank>=s.rank,0)"),
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
                crate::store::ledger::standing::disputed!("p", "decided")
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
pub(crate) struct RunFacts {
    pub(crate) run: Run,
    pub(crate) summary: Summary,
    pub(crate) artifacts: std::path::PathBuf,
}

impl RunFacts {
    /// The orchestrator's section of its runtime state.
    pub(crate) fn state(&self) -> String {
        format!(
            "run: {}\nlive: {}\nworkers: {}\nplan: {}\nartifacts: {}\n",
            self.run.id,
            self.run.options.live,
            self.run.options.workers,
            self.summary.line(),
            self.artifacts.display(),
        )
    }
}

/// A candidate's latest test task, on it or on a candidate merged into it, which has recorded
/// no test since it began: live while it is proving it, abandoned once its job ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OpenTest {
    pub(crate) candidate: CandidateId,
    pub(crate) task: TaskId,
    pub(crate) live: bool,
}

/// Each live candidate's latest test task, read down its own and each merged candidate's tasks
/// newest first, while it recorded no test since; its job's state says whether it is live.
const OPEN_TESTS: &str = concat!(
    "SELECT o.candidate,o.task,NOT j.terminal FROM (SELECT n.id AS candidate,(SELECT \
     max((SELECT tn.task FROM task_node tn WHERE tn.node=m.id AND +tn.phase='test' ORDER BY \
     tn.task DESC LIMIT 1)) FROM node m WHERE m.id=n.id OR m.merged_into=n.id) AS task FROM node \
     n WHERE n.kind='candidate' AND n.merged_into IS NULL) o JOIN task t ON t.id=o.task JOIN \
     job_status j ON j.job=t.job WHERE ",
    crate::store::ledger::facts::untested_since!("o.candidate", "t.began"),
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
pub(crate) struct RunState {
    pub(crate) facts: Arc<RunFacts>,
    pub(crate) coverage: Arc<Coverage>,
    pub(crate) open_tests: Vec<OpenTest>,
}

impl Store {
    pub(crate) fn coverage(&self) -> Result<Coverage> {
        self.read(coverage)
    }

    pub(crate) fn run_facts(&self) -> Result<RunFacts> {
        let (run, summary) = self.read(|db| Ok((runs::run(db)?, summary(db)?)))?;
        Ok(RunFacts {
            run,
            summary,
            artifacts: self.artifact_root(),
        })
    }

    pub(crate) fn open_tests(&self) -> Result<Vec<OpenTest>> {
        self.read(open_tests)
    }

    /// The run as the Research page shows it, read at once.
    pub(crate) fn run_state(&self) -> Result<RunState> {
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
    use super::*;
    use crate::store::tests::{begun, fixture, text, validated};

    /// A test task on a candidate merged away is its survivor's open test.
    #[test]
    fn a_merged_candidates_open_test_is_its_survivors() {
        let (_session, store, actor) = fixture();
        let (merged, kept) = (
            validated(&store, &actor, "a").candidate,
            validated(&store, &actor, "b").candidate,
        );
        let (_, _, task) = begun(&store, &Begin::Test(Listed::of([merged])));
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
}

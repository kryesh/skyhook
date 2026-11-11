//! The synthesis of the run's work: what every phase owes, what runs, what two tasks tried
//! without settling, what the user is asked, what stands unproven, what no path reaches,
//! which interfaces wait on an environment and what lies outside the scope.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::Serialize;
use skyhook::db::{Db, enum_column, params};
use skyhook_base::enums::NamedEnum;
use skyhook_worker::job::JobId;

use crate::store::*;

/// A subject two tasks tried at its current basis, or the scheduler spent there, none live,
/// that still owes the phase.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Exhausted {
    pub(crate) phase: Phase,
    pub(crate) node: NodeId,
    pub(crate) title: String,
    /// The tasks that tried it at its current basis, oldest first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) tasks: Vec<TaskId>,
    /// The last task's child's reply, when a task of it began.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reply: Option<String>,
}

/// A deployment premise for the user to answer: undecided while a review waits on it, or
/// carrying a status proposed within scheduled work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Asked {
    pub(crate) premise: PremiseId,
    pub(crate) title: String,
}

/// A candidate whose unproven verdict stands, with the undecided premises of its chain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Unproven {
    pub(crate) candidate: CandidateId,
    pub(crate) title: String,
    pub(crate) premises: Vec<PremiseRef>,
}

/// An interface that unswept paths reading no caller-supplied state in code wait on: no environment for any
/// candidate serves it, or the launch does no live work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Uncovered {
    pub(crate) interface: InterfaceName,
    /// How many paths wait on it.
    pub(crate) paths: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct Synthesis {
    /// Per phase, the subjects owing it that may still start: no task live, not exhausted.
    pub(crate) owed: BTreeMap<Phase, u64>,
    /// Per phase, the subjects of live tasks.
    pub(crate) running: BTreeMap<Phase, u64>,
    pub(crate) exhausted: Vec<Exhausted>,
    pub(crate) asked: Vec<Asked>,
    pub(crate) unproven: Vec<Unproven>,
    /// Components no path steps through.
    pub(crate) unreached: Vec<ComponentId>,
    pub(crate) uncovered: Vec<Uncovered>,
    pub(crate) outside: Outside,
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
            crate::store::ledger::facts::premise_candidates!("p.node"),
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
pub(in crate::store) use unreached;

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

pub(in crate::store) fn summary(db: &Db) -> Result<Summary> {
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

/// A synthesis as one snapshot finds it, without the replies of its exhausted subjects, with
/// the job of each one's last task: [`Store::synthesis`] reads the replies outside the snapshot.
fn synthesis(db: &Db, spent: &queues::Spent) -> Result<(Synthesis, Vec<Option<JobId>>)> {
    let work = owed_work(db, spent)?;
    let (mut exhausted, mut jobs) = (Vec::new(), Vec::new());
    for (phase, entry) in work.exhausted {
        let tried = rows(
            db,
            "SELECT t.id,t.job FROM task_node tn JOIN task t ON t.id=tn.task WHERE tn.phase=?1 AND \
             tn.node=?2 AND tn.basis=?3 ORDER BY tn.task",
            params![phase, entry.node, entry.basis],
            |row| Ok((TaskId(row.get(0)?), tasks::journal_job(row.get(1)?)?)),
        )?;
        jobs.push(tried.last().map(|(_, job)| *job));
        exhausted.push(Exhausted {
            phase,
            node: entry.node,
            title: entry.title,
            tasks: tried.into_iter().map(|(task, _)| task).collect(),
            reply: None,
        });
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
    let synthesis = Synthesis {
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
    };
    Ok((synthesis, jobs))
}

/// Counts per value of the enum in the first column.
pub(in crate::store) fn counts<T: NamedEnum + Ord>(
    db: &Db,
    sql: &'static str,
) -> Result<BTreeMap<T, u64>> {
    let pairs = rows(db, sql, Vec::new(), |row| {
        Ok((enum_column(row, 0)?, row.get::<u64>(1)?))
    })?;
    Ok(pairs.into_iter().collect())
}

impl Store {
    /// What every phase owes, what runs, what is exhausted with its last child's reply, the
    /// scheduler's `spent` work included, what the user is asked, what stands unproven, what no
    /// path reaches and what lies outside the scope.
    pub(crate) fn synthesis(&self, spent: &[SpentWork]) -> Result<Synthesis> {
        let spent = &queues::Spent::of(spent)?;
        let (mut synthesis, jobs) = self.read(|db| synthesis(db, spent))?;
        for (exhausted, job) in synthesis.exhausted.iter_mut().zip(jobs) {
            let texts = job.map(|job| self.0.agent_job_texts(job)).transpose()?;
            exhausted.reply = texts.flatten().and_then(|texts| texts.reply);
        }
        Ok(synthesis)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::{
        HIGH, assess, begun, component, end_job, fixture, operation, path_from, premise, scenario,
        sweep, text, verdict,
    };
    use crate::store::work::queues::tests::queued;

    /// A deployment premise's status counts when recorded outside every task, as the user's
    /// answer relayed; recorded within scheduled work it only proposes one, which the user is
    /// asked until they answer.
    #[test]
    fn a_deployment_premise_is_answered_outside_scheduled_work() {
        let (_session, store, actor) = fixture();
        let scenario = scenario(&store, &actor, "decoder");
        let deployment = premise(&store, &actor, PremiseScope::Deployment);
        let rests = Edge::FlawPremise {
            flaw: scenario.flaw,
            premise: deployment,
        };
        store.relink(&actor, &[rests], &[]).unwrap();
        let answer = |agent: &AgentId, status| {
            let change = StatusChange::Premise {
                id: deployment,
                status,
            };
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
        let (reviewer, job, _) = begun(&store, &Begin::Review(Listed::of([scenario.candidate])));
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
        let rests = [open, decided].map(|premise| Edge::FlawPremise {
            flaw: scenario.flaw,
            premise,
        });
        store.relink(&actor, &rests, &[]).unwrap();
        let holds = StatusChange::Premise {
            id: decided,
            status: PremiseStatus::Holds,
        };
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
        mark(StatusChange::State(StateMark::Retracted { id: code }));
        mark(StatusChange::State(StateMark::Retracted { id: interface }));
        mark(StatusChange::Component {
            id: idle,
            status: InventoryStatus::Retracted,
        });
        assert_eq!(read(), ((0, 2, 0), vec![], 0, vec![]));
        mark(StatusChange::State(StateMark::Recorded { id: code }));
        assert_eq!(read(), ((1, 2, 0), vec![], 0, swept));
    }
}

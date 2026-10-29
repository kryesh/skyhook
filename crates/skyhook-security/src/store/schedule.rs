//! What the orchestrator's scheduler starts next: the ready queue entries in pipeline
//! order, leaving out what is running, what was tried enough, what running work holds back,
//! later-stage work an earlier-stage unit holds back and the tests it may not start yet, a
//! page at a time. Questions and follow-ups come with the ready units sharing a record with
//! them, which go to one agent.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook::{
    Bounded,
    db::{Db, params},
    identity::JobId,
};

use super::*;

/// Work tried enough on one basis; the same subject on another basis is new work.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Attempt {
    pub subject: WorkSubject,
    #[serde(default)]
    pub basis: Option<Basis>,
}

/// Where a page of startable work continues: after key `after` of `phase`'s queue.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    pub phase: Phase,
    pub after: i64,
}

/// An entry to start now, with the ready units its agent takes too.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Startable {
    pub phase: Phase,
    #[serde(flatten)]
    pub entry: QueueEntry,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bundle: Vec<QueueEntry>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Startables {
    pub items: Vec<Startable>,
    /// Where the next page continues; none at the end.
    pub next: Option<Cursor>,
    /// A test the page passed over waits for an approval, an unblock or a slot.
    pub held: bool,
}

/// The tests the scheduler may start now; the rest of the poc and poc_pending work waits.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Tests {
    /// How many may start; none when omitted.
    #[serde(default)]
    pub slots: u16,
    /// Candidates the user approved a test of, under the launch's `ask` policy.
    #[serde(default)]
    pub approved: Vec<CandidateId>,
    /// Candidates whose pending test may start again.
    #[serde(default)]
    pub unblocked: Vec<CandidateId>,
}

impl Tests {
    /// Whether `entry`'s test may start under `policy`, slots aside; None for other work.
    fn admits(&self, policy: PocPolicy, phase: Phase, entry: &QueueEntry) -> Option<bool> {
        let WorkSubject::Candidate(candidate) = entry.subject else {
            return None;
        };
        match phase {
            Phase::Poc => Some(policy == PocPolicy::Yes || self.approved.contains(&candidate)),
            Phase::PocPending => Some(self.unblocked.contains(&candidate)),
            _ => None,
        }
    }
}

/// The ids of the units naming a record of the lists `?1`-`?5`, as [`named`] binds them:
/// each kind is matched where it is stored, so every branch reads an index. A filter on
/// `unit_about` reaches every arm of the view, scanning those that cannot match.
macro_rules! naming {
    () => {
        "SELECT id FROM plan_unit WHERE flaw IN (SELECT value FROM json_each(?1)) UNION ALL \
         SELECT id FROM plan_unit WHERE candidate IN (SELECT value FROM json_each(?2)) UNION ALL \
         SELECT unit FROM unit_invariant WHERE guarantee IN (SELECT value FROM json_each(?3)) \
         UNION ALL SELECT unit FROM unit_invariant WHERE premise IN (SELECT value FROM \
         json_each(?4)) UNION ALL SELECT k.unit FROM json_each(?5) j JOIN contract c ON \
         c.component=j.value->>0 AND c.function=j.value->>1 JOIN unit_check k ON k.contract=c.id"
    };
}

/// Unit `u` is one of the kinds `?6` lists, open and ready.
macro_rules! ready_of {
    () => {
        concat!(
            "u.kind IN (SELECT value FROM json_each(?6)) AND ",
            queues::open_unit!("u"),
            " AND ",
            queues::unit_ready!()
        )
    };
}

/// Records as the five lists [`naming`] matches each kind of by its own columns.
fn named<'a>(records: impl IntoIterator<Item = &'a About>) -> Result<Vec<libsql::Value>> {
    let (mut flaws, mut candidates, mut guarantees, mut premises, mut functions) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for record in records {
        match record {
            About::Flaw(id) => flaws.push(id.0),
            About::Candidate(id) => candidates.push(id.0),
            About::Guarantee(id) => guarantees.push(id.0),
            About::Premise(id) => premises.push(id.0),
            About::Function {
                component,
                function,
            } => functions.push((component.0, function)),
        }
    }
    Ok(params![
        json_list(&flaws)?,
        json_list(&candidates)?,
        json_list(&guarantees)?,
        json_list(&premises)?,
        json_list(&functions)?
    ])
}

/// Work that holds other work back: what is running or about to start.
struct Work {
    subject: WorkSubject,
    about: Vec<About>,
}

impl Work {
    fn of(entry: &QueueEntry) -> Self {
        Self {
            subject: entry.subject,
            about: entry.about.clone(),
        }
    }

    /// Running work holds the units it touches, and running units the work touching them.
    fn holds(&self, entry: &QueueEntry) -> bool {
        let unit = |subject| matches!(subject, WorkSubject::Unit(_));
        self.subject == entry.subject
            || ((unit(self.subject) || unit(entry.subject))
                && self.about.iter().any(|about| entry.about.contains(about)))
    }
}

/// The unit phases of `phases` as a JSON list of their unit kinds.
fn kinds(phases: impl Iterator<Item = Phase>) -> Result<String> {
    json_list(&phases.filter_map(Phase::unit).collect::<Vec<_>>())
}

impl Store {
    /// Live root workflows whose latest scheduling page held tests but offered no work,
    /// while the session has no active child agents. Read from the journal on every root request:
    /// job completion does not change the research generation. The scheduling call identifies
    /// the workflow without relying on a script's name or source, even beside a polling script.
    pub(crate) fn waiting_workflows(&self) -> Result<Vec<JobId>> {
        self.read(|db| {
            rows(
                db,
                "SELECT script.id FROM job script JOIN entry e ON e.seq=script.created \
                 JOIN agent owner ON owner.id=e.agent AND owner.parent IS NULL \
                 JOIN job_status ss ON ss.job=script.id JOIN job_state sk ON sk.name=ss.state \
                 JOIN job query ON query.id=(SELECT q.id FROM job q LEFT JOIN tool_call t \
                     ON t.item=q.origin_call WHERE q.parent=script.id AND q.tool='research_query' \
                     AND json_extract(coalesce(q.arguments,t.arguments),'$.action')='startable' \
                     ORDER BY q.id DESC LIMIT 1) \
                 JOIN job_status qs ON qs.job=query.id AND qs.state='completed' \
                 JOIN job_output o ON o.job=query.id AND o.generation=(SELECT max(generation) \
                     FROM job_output WHERE job=query.id) \
                 WHERE script.role='script' AND NOT sk.terminal \
                 AND json_extract(o.result,'$.held')=1 AND json_array_length(o.result,'$.items')=0 \
                 AND json_extract(o.result,'$.next') IS NULL \
                 AND NOT EXISTS (SELECT 1 FROM job child JOIN job_status cs ON cs.job=child.id \
                     JOIN job_state ck ON ck.name=cs.state WHERE child.role='agent' AND NOT ck.terminal) \
                 ORDER BY script.id",
                Vec::new(),
                |row| {
                    JobId::new(row.get::<i64>(0)? as u64)
                        .map_err(|_| corrupt("invalid workflow job id").into())
                },
            )
        })
    }

    /// The next page of work that can start now, earliest pipeline stage first:
    /// none of `running`, no attempt in `spent`, nothing running work holds back or a
    /// ready earlier-stage unit touches, and no test beyond what `tests` admits. A question
    /// or follow-up carries every other such unit sharing a record with it, and what one page
    /// starts holds back the rest of it as running work would. Reading reserves nothing.
    pub fn startable(
        &self,
        running: &[WorkSubject],
        spent: &[Attempt],
        tests: &Tests,
        after: Option<Cursor>,
        limit: Bounded<1, 1000>,
    ) -> Result<Startables> {
        let mut phases = Phase::ALL.to_vec();
        phases.sort_by_key(|phase| phase.stage());
        let first = match after {
            Some(cursor) => phases.iter().position(|phase| *phase == cursor.phase),
            None => Some(0),
        };
        let bundled = kinds((phases.iter().copied()).filter(|phase| phase.bundled()))?;
        let wanted = u64::from(limit) as usize;
        self.read(|db| {
            let policy = runs::run(db)?.poc;
            let mut busy = running_work(db, running)?;
            let (mut items, mut slots, mut held) = (Vec::new(), tests.slots, false);
            for &phase in phases.iter().skip(first.unwrap_or(phases.len())) {
                let tested = matches!(phase, Phase::Poc | Phase::PocPending);
                // With no slot left, the first test passed over shows that tests wait.
                if tested && slots == 0 && held {
                    continue;
                }
                let earlier = kinds(
                    (phases.iter().copied()).filter(|earlier| earlier.stage() < phase.stage()),
                )?;
                // Tests are paged whole: the ones `tests` admits may lie far behind the rest.
                let mut page = Page::new(
                    after
                        .filter(|cursor| cursor.phase == phase)
                        .map_or(0, |cursor| cursor.after),
                    if tested {
                        Bounded::new::<1000>()
                    } else {
                        limit
                    },
                );
                'phase: loop {
                    let listing = queues::page(db, phase, page)?;
                    let touched = touched(db, &listing, &earlier)?;
                    for entry in listing.items {
                        let key = entry.key();
                        if spent.iter().any(|attempt| {
                            attempt.subject == entry.subject && attempt.basis == entry.basis
                        }) || busy.iter().any(|work| work.holds(&entry))
                            || entry.about.iter().any(|about| touched.contains(about))
                        {
                            continue;
                        }
                        if let Some(admitted) = tests.admits(policy, phase, &entry) {
                            if !admitted || slots == 0 {
                                held = true;
                                if slots == 0 {
                                    break 'phase;
                                }
                                continue;
                            }
                            slots -= 1;
                        }
                        let bundle = match phase.bundled() {
                            true => partners(db, &entry, &bundled, &busy, spent)?,
                            false => Vec::new(),
                        };
                        busy.push(Work::of(&entry));
                        busy.extend(bundle.iter().map(Work::of));
                        items.push(Startable {
                            phase,
                            entry,
                            bundle,
                        });
                        if items.len() == wanted {
                            return Ok(Startables {
                                items,
                                next: Some(Cursor { phase, after: key }),
                                held,
                            });
                        }
                    }
                    match listing.next_after {
                        Some(after) => page.after = after,
                        None => break,
                    }
                }
            }
            Ok(Startables {
                items,
                next: None,
                held,
            })
        })
    }
}

/// What each running subject is about.
fn running_work(db: &Db, running: &[WorkSubject]) -> Result<Vec<Work>> {
    let (mut units, mut candidates, mut contracts) = (Vec::new(), Vec::new(), Vec::new());
    for subject in running {
        match subject {
            WorkSubject::Unit(id) => units.push(*id),
            WorkSubject::Candidate(id) => candidates.push(*id),
            WorkSubject::Contract(id) => contracts.push(*id),
            WorkSubject::Root(_) => {}
        }
    }
    let (mut units, mut candidates, mut contracts) = (
        queues::units_about(db, &units)?,
        queues::candidates_about(db, &candidates)?,
        queues::contracts_about(db, &contracts)?,
    );
    Ok((running.iter())
        .map(|subject| Work {
            subject: *subject,
            about: match subject {
                WorkSubject::Unit(id) => units.remove(id),
                WorkSubject::Candidate(id) => candidates.remove(id),
                WorkSubject::Contract(id) => contracts.remove(id),
                WorkSubject::Root(_) => None,
            }
            .unwrap_or_default(),
        })
        .collect())
}

/// The records of `listing`'s entries that a ready unit of the `earlier` kinds names.
fn touched(db: &Db, listing: &Listing<QueueEntry>, earlier: &str) -> Result<BTreeSet<About>> {
    let records: Vec<&About> = (listing.items.iter())
        .flat_map(|entry| &entry.about)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if records.is_empty() {
        return Ok(BTreeSet::new());
    }
    let mut params = named(records.iter().copied())?;
    params.extend(params![earlier]);
    Ok(rows(
        db,
        concat!(
            "SELECT flaw,candidate,guarantee,premise,component,function FROM unit_about WHERE \
             unit IN (SELECT u.id FROM plan_unit u WHERE u.id IN (",
            naming!(),
            ") AND ",
            ready_of!(),
            ")"
        ),
        params,
        |row| queues::about_columns(row, 0),
    )?
    .into_iter()
    .filter(|about| records.contains(&about))
    .collect())
}

/// The other ready units of the `bundled` kinds sharing a record with `head` that nothing
/// holds back, in id order.
fn partners(
    db: &Db,
    head: &QueueEntry,
    bundled: &str,
    busy: &[Work],
    spent: &[Attempt],
) -> Result<Vec<QueueEntry>> {
    let mut params = named(&head.about)?;
    params.extend(params![bundled]);
    let found = rows(
        db,
        concat!(
            "SELECT u.id,u.question FROM plan_unit u WHERE u.id IN (",
            naming!(),
            ") AND ",
            ready_of!(),
            " ORDER BY u.id"
        ),
        params,
        |row| Ok((UnitId(row.get(0)?), row.get::<String>(1)?)),
    )?;
    let ids: Vec<UnitId> = found.iter().map(|(id, _)| *id).collect();
    let mut about = queues::units_about(db, &ids)?;
    Ok((found.into_iter())
        .map(|(id, title)| QueueEntry {
            subject: WorkSubject::Unit(id),
            title,
            basis: None,
            about: about.remove(&id).unwrap_or_default(),
        })
        .filter(|unit| {
            unit.subject != head.subject
                && !busy.iter().any(|work| work.holds(unit))
                && !spent
                    .iter()
                    .any(|attempt| attempt.subject == unit.subject && attempt.basis.is_none())
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{
        super::tests::{fixture, follow_up_unit, shortlisted, text},
        *,
    };

    /// The latest scheduling page, not names or a count of scripts, identifies a held
    /// workflow; live child agents and pages offering work or unheld tests do not.
    #[test]
    fn waiting_workflows_follow_the_latest_page_and_live_jobs() {
        use super::super::tests::{child_agent, end_job, journal_job};
        use skyhook::job::JobState;

        let (_session, store, _) = fixture();
        let held = Startables {
            items: Vec::new(),
            next: None,
            held: true,
        };
        let workflow = journal_job(&store, None, None);
        let _poller = journal_job(&store, None, None);
        assert!(store.waiting_workflows().unwrap().is_empty());
        journal_job(&store, Some(workflow), Some(&held));
        assert_eq!(store.waiting_workflows().unwrap(), [workflow]);

        for state in [JobState::Queued, JobState::Running, JobState::WaitingInput] {
            let (_, child) = child_agent(&store, state);
            assert!(store.waiting_workflows().unwrap().is_empty());
            end_job(&store, child);
            assert_eq!(store.waiting_workflows().unwrap(), [workflow]);
        }

        let ready = Startables {
            items: vec![Startable {
                phase: Phase::Poc,
                entry: QueueEntry {
                    subject: WorkSubject::Candidate(CandidateId(1)),
                    title: "ready".into(),
                    basis: None,
                    about: Vec::new(),
                },
                bundle: Vec::new(),
            }],
            next: None,
            held: true,
        };
        journal_job(&store, Some(workflow), Some(&ready));
        assert!(store.waiting_workflows().unwrap().is_empty());
        let unheld = Startables {
            items: Vec::new(),
            next: None,
            held: false,
        };
        journal_job(&store, Some(workflow), Some(&unheld));
        assert!(store.waiting_workflows().unwrap().is_empty());
        let paged = Startables {
            items: Vec::new(),
            next: Some(Cursor {
                phase: Phase::Poc,
                after: 1,
            }),
            held: true,
        };
        journal_job(&store, Some(workflow), Some(&paged));
        assert!(store.waiting_workflows().unwrap().is_empty());
        journal_job(&store, Some(workflow), Some(&held));
        assert_eq!(store.waiting_workflows().unwrap(), [workflow]);

        end_job(&store, workflow.get() as i64);
        assert!(store.waiting_workflows().unwrap().is_empty());
    }

    /// Ready work starts in pipeline order: follow-ups sharing a candidate go to one agent
    /// and hold back that candidate's review, and running or spent work starts nothing.
    #[test]
    fn startable_work_follows_the_pipeline_and_its_holds() {
        let (_directory, store, actor) = fixture();
        let candidate = shortlisted(&store, &actor);
        let units = [(); 2].map(|()| {
            let work = follow_up_unit(Some(candidate), None, Vec::new());
            store.plan_unit(&actor, &work).unwrap()
        });
        let [first, second] = units.map(WorkSubject::Unit);
        let started = |running: &[WorkSubject], spent: &[Attempt]| {
            let page = store
                .startable(
                    running,
                    spent,
                    &Tests::default(),
                    None,
                    Bounded::new::<10>(),
                )
                .unwrap();
            (page.items.into_iter())
                .map(|item| {
                    let bundle = item.bundle.iter().map(|unit| unit.subject).collect();
                    (item.entry.subject, bundle)
                })
                .collect::<Vec<(WorkSubject, Vec<WorkSubject>)>>()
        };
        assert_eq!(started(&[], &[]), [(first, vec![second])]);
        assert!(started(&[first], &[]).is_empty());
        let spent = [first, second].map(|subject| Attempt {
            subject,
            basis: None,
        });
        assert!(
            started(&[], &spent).is_empty(),
            "ready units hold the review"
        );
        for unit in units {
            store
                .record_outcome(&actor, unit, Outcome::Blocked, &text("stub"), &[])
                .unwrap();
        }
        assert_eq!(
            started(&[], &spent),
            [(WorkSubject::Candidate(candidate), Vec::new())]
        );
    }
}

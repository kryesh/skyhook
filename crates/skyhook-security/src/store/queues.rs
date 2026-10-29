//! Work a pool reads, derived from the records: open plan units of one kind, candidates
//! owing an assessment, a review or a test, and root flaws owing a report. A queue lists
//! what is owed and ready now; its count is everything owed, ready or not, so work waiting
//! on another unit stays counted.

use std::collections::BTreeMap;

use libsql::Row;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook::db::{Db, params};

use super::*;

/// What a queue entry is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum WorkSubject {
    Unit(UnitId),
    Candidate(CandidateId),
    /// A root flaw, whose group of candidates is reported as one finding.
    Root(FlawId),
    Contract(ContractId),
}

/// What a candidate entry's work rests on: the same subject on another basis is new work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Basis {
    Assess {
        revision: u32,
    },
    Review {
        assessment: AssessmentId,
        verdict: Option<ValidationId>,
    },
    Test {
        validation: ValidationId,
    },
    /// The root's report generation: the scenarios its group would publish now.
    Report {
        generation: ReportKey,
    },
}

/// A record a queue entry's work is about. [`Store::startable`] gives units sharing one to
/// one agent, holds back later-stage work an earlier-stage unit shares one with, holds back
/// a unit about a record running work is about, and holds a candidate's work back on units
/// only, so candidates sharing a member flaw run side by side. A check is also about the
/// function it asks about, so one function's checks go to one agent.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum About {
    Flaw(FlawId),
    Candidate(CandidateId),
    Guarantee(GuaranteeId),
    Premise(PremiseId),
    Function {
        component: ComponentId,
        function: String,
    },
}

impl From<Invariant> for About {
    fn from(invariant: Invariant) -> Self {
        match invariant {
            Invariant::Guarantee(id) => Self::Guarantee(id),
            Invariant::Premise(id) => Self::Premise(id),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct QueueEntry {
    pub subject: WorkSubject,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basis: Option<Basis>,
    /// What a question or follow-up is about, or a candidate's work: the candidate, its
    /// member flaws and its premises.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub about: Vec<About>,
}

impl Keyed for QueueEntry {
    fn key(&self) -> i64 {
        match self.subject {
            WorkSubject::Unit(id) => id.0,
            WorkSubject::Candidate(id) => id.0,
            WorkSubject::Root(id) => id.0,
            WorkSubject::Contract(id) => id.0,
        }
    }
}

/// Unit `$u` is open.
macro_rules! open_unit {
    ($u:literal) => {
        concat!("NOT EXISTS (SELECT 1 FROM outcome WHERE unit=", $u, ".id)")
    };
}
pub(super) use open_unit;

/// A sweep of component `$component` is open.
macro_rules! open_sweep {
    ($component:literal) => {
        concat!(
            "EXISTS (SELECT 1 FROM plan_unit w WHERE w.component=",
            $component,
            " AND ",
            queues::open_unit!("w"),
            ")"
        )
    };
}
pub(super) use open_sweep;

/// The latest test of candidate `c` that still stands is pending.
macro_rules! pending_test {
    () => {
        "ifnull((SELECT p.result FROM poc p WHERE p.candidate=c.id AND \
         p.contradictions=c.contradictions ORDER BY p.id DESC LIMIT 1)='pending',0)"
    };
}

/// The launch working the run tests validated candidates.
macro_rules! testing_allowed {
    () => {
        "(SELECT poc FROM run)!='no'"
    };
}

/// Event `$event` came after candidate `c`'s latest settled test at its contradictions.
macro_rules! since_test {
    ($event:literal) => {
        concat!(
            "ifnull(",
            $event,
            ",0)>(SELECT event FROM event_record WHERE poc=(SELECT max(id) FROM poc WHERE \
             candidate=c.id AND contradictions=c.contradictions AND result!='pending'))"
        )
    };
}

/// Unless the launch declined testing, a positive verdict owes a test until one settles it,
/// published or not, unless it is noted. A settled test is owed again only once what it
/// depended on changed after the latest attempt: an environment recorded since one the
/// environment could not run or one that failed, or the candidate assessed since one that
/// failed. A declined test stands until the user asks again. A pending test waits on the user
/// or the environment instead.
macro_rules! owes_test {
    () => {
        concat!(
            "(",
            testing_allowed!(),
            " AND NOT ",
            pending_test!(),
            " AND st.state='validated' AND (st.poc IS NULL OR (st.result IN ('failed',\
             'unavailable') AND ",
            since_test!("(SELECT max(event) FROM event_record WHERE environment IS NOT NULL)"),
            ") OR (st.result='failed' AND ",
            since_test!("(SELECT event FROM event_record WHERE assessment=st.assessment)"),
            ")))"
        )
    };
}

/// Some scenario of root `$root`'s group is reportable; the latest finding of a root with
/// none is withdrawn.
macro_rules! group_reportable {
    ($root:literal) => {
        concat!(
            "EXISTS (SELECT 1 FROM group_scenario WHERE root=",
            $root,
            ")"
        )
    };
}
pub(super) use group_reportable;

/// Candidate `c` owes an assessment.
macro_rules! owes_assessment {
    () => {
        "st.status='identified'"
    };
}

/// An open follow-up concerns candidate `c`, or an open unit asks about a condition it relies
/// on: its tests wait until the unit ends.
macro_rules! questioned {
    () => {
        concat!(
            "(EXISTS (SELECT 1 FROM plan_unit u WHERE u.candidate=c.id AND ",
            queues::open_unit!("u"),
            ") OR EXISTS (SELECT 1 FROM candidate_condition qc JOIN unit_invariant qi ON \
             qi.premise=qc.premise JOIN plan_unit u ON u.id=qi.unit WHERE qc.candidate=c.id AND ",
            queues::open_unit!("u"),
            "))"
        )
    };
}
pub(super) use questioned;

/// Candidate `c`'s unproven verdict waits: a unit [`questioned`] names is open, or a condition
/// it relies on is asked of the user under the current scope.
macro_rules! waits {
    () => {
        concat!("(", questioned!(), " OR ", premises::awaits_answer!(), ")")
    };
}
pub(super) use waits;

/// Shortlisted candidates owe review. An unproven verdict waited on work when it was
/// recorded, and is owed review again once nothing it [`waits`] on remains.
macro_rules! owes_review {
    () => {
        concat!(
            "(st.status='shortlisted' OR (st.status='unproven' AND NOT ",
            waits!(),
            "))"
        )
    };
}

/// No candidate sharing a root with candidate `c` comes before it for a test: an untested
/// positive verdict that is more severe, or as severe and older. A root group's first
/// demonstration goes to its most severe scenario, and what it confirms notes the rest; a
/// failed one holds nothing back, so the next most severe untested scenario follows it.
macro_rules! first_in_groups {
    () => {
        "NOT EXISTS (SELECT 1 FROM candidate_root g JOIN candidate_root o ON o.root=g.root AND \
         o.candidate!=g.candidate JOIN candidate_state d ON d.candidate=o.candidate WHERE \
         g.candidate=c.id AND d.state='validated' AND d.poc IS NULL AND (d.rank>st.rank OR \
         (d.rank=st.rank AND d.candidate<c.id)))"
    };
}

/// Nothing holds candidate `c`'s finding work back: no path that is its own or reaches a
/// member's component has an open review, and no member's component an open sweep.
macro_rules! settled {
    () => {
        concat!(
            "NOT EXISTS (SELECT 1 FROM plan_unit u WHERE ",
            open_unit!("u"),
            " AND u.path IN (SELECT path FROM candidate_path WHERE candidate=c.id UNION SELECT \
             r.path FROM candidate_member m JOIN flaw f ON f.id=m.flaw JOIN path_reach r ON \
             r.component=f.component WHERE m.candidate=c.id)) AND NOT EXISTS (SELECT 1 FROM \
             candidate_member m JOIN flaw f ON f.id=m.flaw WHERE m.candidate=c.id AND ",
            open_sweep!("f.component"),
            ")"
        )
    };
}

/// A phase's entries past `?1`, limit `?2`: what it owes (`owed`) and, for the page, what
/// of that is ready (`ready`). Each row is a key, a title and up to two basis ids; a unit
/// phase binds its kind as `?3`.
macro_rules! phase_sql {
    ($select:expr, $key:literal, $owed:expr, $ready:expr) => {
        (
            concat!(
                $select,
                " WHERE ",
                $key,
                ">?1 AND ",
                $owed,
                " AND ",
                $ready,
                " ORDER BY ",
                $key,
                " LIMIT ?2"
            ),
            concat!(
                "SELECT count(*) FROM (",
                $select,
                " WHERE ",
                $key,
                ">?1 AND ",
                $owed,
                " LIMIT ?2)"
            ),
        )
    };
}

macro_rules! candidates {
    ($basis:literal) => {
        concat!(
            "SELECT c.id,c.title,",
            $basis,
            " FROM candidate c JOIN candidate_standing st ON st.candidate=c.id"
        )
    };
}

/// Unit `u`'s prerequisites have ended. A path review waits for the sweeps of the components
/// its path reaches, and a check for its component's sweep.
macro_rules! unit_ready {
    () => {
        concat!(
            "(u.kind!='path_review' OR NOT EXISTS (SELECT 1 FROM path_reach r WHERE r.path=u.path \
             AND ",
            queues::open_sweep!("r.component"),
            ")) AND NOT EXISTS (SELECT 1 FROM unit_check k JOIN contract c ON c.id=k.contract \
             WHERE k.unit=u.id AND ",
            queues::open_sweep!("c.component"),
            ")"
        )
    };
}
pub(super) use unit_ready;

/// A phase's queries, the kind of open unit a unit phase binds beyond the page, and what its
/// keys name.
struct Query {
    page: &'static str,
    count: &'static str,
    unit: Option<UnitKind>,
    subject: fn(i64) -> WorkSubject,
    basis: BasisOf,
}

/// How a phase's row names what its work rests on.
type BasisOf = fn(&Row) -> Result<Option<Basis>>;

fn query(phase: Phase) -> Query {
    let ((page, count), unit, basis): ((&str, &str), Option<UnitKind>, BasisOf) = match phase {
        Phase::Assess => (
            phase_sql!(
                candidates!("c.revision,NULL"),
                "c.id",
                owes_assessment!(),
                settled!()
            ),
            None,
            |row| {
                Ok(Some(Basis::Assess {
                    revision: row.get::<i64>(2)? as u32,
                }))
            },
        ),
        Phase::Review => (
            phase_sql!(
                candidates!(
                    "st.assessment,(SELECT max(id) FROM validation WHERE assessment=st.assessment)"
                ),
                "c.id",
                owes_review!(),
                settled!()
            ),
            None,
            |row| {
                Ok(Some(Basis::Review {
                    assessment: AssessmentId(row.get(2)?),
                    verdict: row.get::<Option<i64>>(3)?.map(ValidationId),
                }))
            },
        ),
        Phase::Poc => (
            phase_sql!(
                candidates!("st.validation,NULL"),
                "c.id",
                owes_test!(),
                concat!(
                    settled!(),
                    " AND NOT ",
                    questioned!(),
                    " AND ",
                    first_in_groups!()
                )
            ),
            None,
            |row| {
                Ok(Some(Basis::Test {
                    validation: ValidationId(row.get(2)?),
                }))
            },
        ),
        Phase::PocPending => (
            phase_sql!(
                candidates!("st.validation,NULL"),
                "c.id",
                concat!(
                    testing_allowed!(),
                    " AND st.state='validated' AND ",
                    pending_test!()
                ),
                concat!(settled!(), " AND NOT ", questioned!())
            ),
            None,
            |row| {
                Ok(Some(Basis::Test {
                    validation: ValidationId(row.get(2)?),
                }))
            },
        ),
        // Owed while some scenario of the root's group is reportable and the root's latest
        // finding, if any, does not publish them as they stand; ready once no scenario of the
        // group owes an assessment, a review or a test, pending or open.
        Phase::Report => (
            phase_sql!(
                "SELECT f.id,f.title,NULL,NULL FROM flaw f",
                "f.id",
                concat!(
                    group_reportable!("f.id"),
                    " AND NOT EXISTS (SELECT 1 FROM finding l WHERE l.root=f.id AND ",
                    report::latest_finding!("l"),
                    " AND ",
                    report::fresh!("l"),
                    ")"
                ),
                concat!(
                    "NOT EXISTS (SELECT 1 FROM candidate_root g JOIN candidate c ON c.id=g.candidate \
                     JOIN candidate_standing st ON st.candidate=c.id WHERE g.root=f.id AND (",
                    owes_assessment!(),
                    " OR ",
                    owes_review!(),
                    " OR ",
                    owes_test!(),
                    " OR (",
                    testing_allowed!(),
                    " AND ",
                    pending_test!(),
                    ") OR EXISTS (SELECT 1 FROM poc_start s WHERE s.candidate=c.id AND ",
                    workflow::open_test!(),
                    ")))"
                )
            ),
            None,
            // Filled from the page's generations, read at once.
            |_| Ok(None),
        ),
        // Ready once its component's sweep ended.
        Phase::Check => (
            phase_sql!(
                "SELECT c.id,printf('%s in component #%d (%s)',c.function,c.component,c.kind),\
                 NULL,NULL FROM contract_standing c",
                "c.id",
                checks::owed!(),
                concat!("NOT ", open_sweep!("c.component"))
            ),
            None,
            |_| Ok(None),
        ),
        unit => (
            phase_sql!(
                "SELECT u.id,coalesce(u.question,(SELECT name FROM component WHERE id=u.component),\
                     (SELECT description FROM path WHERE id=u.path)),\
                     NULL,NULL FROM plan_unit u",
                "u.id",
                concat!("u.kind=?3 AND ", open_unit!("u")),
                unit_ready!()
            ),
            Some(unit.unit().expect("every other phase pools plan units")),
            |_| Ok(None),
        ),
    };
    Query {
        page,
        count,
        unit,
        subject: match (phase, phase.unit()) {
            (_, Some(_)) => |id| WorkSubject::Unit(UnitId(id)),
            (Phase::Report, None) => |id| WorkSubject::Root(FlawId(id)),
            (Phase::Check, None) => |id| WorkSubject::Contract(ContractId(id)),
            (_, None) => |id| WorkSubject::Candidate(CandidateId(id)),
        },
        basis,
    }
}

impl Query {
    fn params(&self, mut params: Vec<libsql::Value>) -> Vec<libsql::Value> {
        params.extend(self.unit.map(|kind| params![kind]).unwrap_or_default());
        params
    }
}

/// How many entries `phase` owes, ready or not.
pub(super) fn tally(db: &Db, phase: Phase) -> Result<u64> {
    let query = query(phase);
    count(db, query.count, query.params(params![0_i64, -1_i64]))
}

/// The next page of `phase` work that can start now, each entry with what it is about and
/// what it rests on: a unit, candidate or root whose prerequisites are still open waits off
/// the page while it stays owed.
pub(super) fn page(db: &Db, phase: Phase, page: Page) -> Result<Listing<QueueEntry>> {
    let query = query(phase);
    let params = query.params(params![page.after, page.limit()]);
    let entries = rows(db, query.page, params, |row| {
        Ok(QueueEntry {
            subject: (query.subject)(row.get(0)?),
            title: row.get(1)?,
            basis: (query.basis)(row)?,
            about: Vec::new(),
        })
    })?;
    let (mut units, mut candidates, mut roots, mut contracts) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for entry in &entries {
        match entry.subject {
            WorkSubject::Unit(id) => units.push(id),
            WorkSubject::Candidate(id) => candidates.push(id),
            WorkSubject::Root(id) => roots.push(id),
            WorkSubject::Contract(id) => contracts.push(id),
        }
    }
    let (mut units, mut candidates, mut generations, mut contracts) = (
        units_about(db, &units)?,
        candidates_about(db, &candidates)?,
        report::generations(db, &roots)?,
        contracts_about(db, &contracts)?,
    );
    let entries = (entries.into_iter())
        .map(|mut entry| {
            match entry.subject {
                WorkSubject::Unit(id) => entry.about = units.remove(&id).unwrap_or_default(),
                WorkSubject::Candidate(id) => {
                    entry.about = candidates.remove(&id).unwrap_or_default()
                }
                WorkSubject::Root(id) => {
                    entry.basis = (generations.remove(&id)).map(|generation| Basis::Report {
                        generation: generation.key,
                    })
                }
                WorkSubject::Contract(id) => {
                    entry.about = contracts.remove(&id).unwrap_or_default()
                }
            }
            entry
        })
        .collect();
    Ok(Listing::keyset(entries, page))
}

/// What each of `candidates` is about, read in one query: the candidate, its member flaws
/// and the conditions it relies on.
pub(super) fn candidates_about(
    db: &Db,
    candidates: &[CandidateId],
) -> Result<BTreeMap<CandidateId, Vec<About>>> {
    let mut about: BTreeMap<_, _> = (candidates.iter())
        .map(|id| (*id, vec![About::Candidate(*id)]))
        .collect();
    for (candidate, record) in rows(
        db,
        "SELECT j.value,m.flaw,NULL FROM json_each(?1) j JOIN candidate_member m ON \
         m.candidate=j.value UNION ALL SELECT j.value,NULL,cc.premise FROM json_each(?1) j JOIN \
         candidate_condition cc ON cc.candidate=j.value ORDER BY 3,2",
        params![json_list(candidates)?],
        |row| {
            let record = match row.get::<Option<i64>>(1)? {
                Some(flaw) => About::Flaw(FlawId(flaw)),
                None => About::Premise(PremiseId(row.get(2)?)),
            };
            Ok((CandidateId(row.get(0)?), record))
        },
    )? {
        about.entry(candidate).or_default().push(record);
    }
    Ok(about)
}

/// What each of `contracts` is about: the function it is kept by.
pub(super) fn contracts_about(
    db: &Db,
    contracts: &[ContractId],
) -> Result<BTreeMap<ContractId, Vec<About>>> {
    let about = rows(
        db,
        "SELECT id,component,function FROM contract WHERE id IN (SELECT value FROM json_each(?1))",
        params![json_list(contracts)?],
        |row| {
            let function = About::Function {
                component: ComponentId(row.get(1)?),
                function: row.get(2)?,
            };
            Ok((ContractId(row.get(0)?), vec![function]))
        },
    )?;
    Ok(about.into_iter().collect())
}

/// The record a `unit_about` row from `offset` names.
pub(super) fn about_columns(row: &Row, offset: i32) -> Result<About> {
    let id = |index| row.get::<Option<i64>>(offset + index);
    Ok(if let Some(flaw) = id(0)? {
        About::Flaw(FlawId(flaw))
    } else if let Some(candidate) = id(1)? {
        About::Candidate(CandidateId(candidate))
    } else if let Some(guarantee) = id(2)? {
        About::Guarantee(GuaranteeId(guarantee))
    } else if let Some(premise) = id(3)? {
        About::Premise(PremiseId(premise))
    } else {
        About::Function {
            component: ComponentId(row.get(offset + 4)?),
            function: row.get(offset + 5)?,
        }
    })
}

/// What each of `units` is about, read in one query.
pub(super) fn units_about(db: &Db, units: &[UnitId]) -> Result<BTreeMap<UnitId, Vec<About>>> {
    let mut work = BTreeMap::<_, Vec<About>>::new();
    for (unit, record) in rows(
        db,
        "SELECT unit,flaw,candidate,guarantee,premise,component,function FROM unit_about WHERE \
         unit IN (SELECT value FROM json_each(?1)) ORDER BY unit,rank,position",
        params![json_list(units)?],
        |row| Ok((UnitId(row.get(0)?), about_columns(row, 1)?)),
    )? {
        let about = work.entry(unit).or_default();
        if !about.contains(&record) {
            about.push(record);
        }
    }
    Ok(work)
}

#[cfg(test)]
mod tests {
    use super::{
        super::tests::{
            HIGH, answerer, assess, assess_at, candidate, child_agent, component, confidence,
            contradiction, contributes, entries, environment, fixture, flaw_revision,
            follow_up_unit, holds, member, new_candidate, new_flaw, new_premise, path, poc, queued,
            report_input, reviewed, root_of, scenario, shortlisted, snapshot, status, text, units,
            validated, verdict,
        },
        *,
    };
    use skyhook::job::JobState;

    /// After a material change, a re-assessment with the vector and conditions a verdict
    /// judged carries that verdict and its test stays in force; other conditions owe review.
    #[test]
    fn an_unchanged_reassessment_carries_the_verdict_and_its_test() {
        let (_directory, store, actor) = fixture();
        let id = validated(&store, &actor);
        let (flaw, path) = member(&store, id);
        poc(&store, &actor, id, PocResult::Confirmed);
        let change = || {
            store
                .add_evidence(
                    &actor,
                    EvidenceSubject::Flaw(flaw),
                    &contradiction("narrower"),
                )
                .unwrap();
            contributes(&store, &actor, flaw, path);
            assess(&store, &actor, id);
        };
        change();
        assert_eq!(confidence(&store, id), Some(ConfidenceTier::Demonstrated));
        assert!(queued(&store, Phase::Review).is_empty());

        let premise = store
            .add_premise(
                &actor,
                scenario(id),
                &new_premise(InvariantKind::State, "the stream resumes"),
            )
            .unwrap();
        holds(&store, &actor, premise);
        change();
        assert_eq!(status(&store, id), Status::Shortlisted);
        assert_eq!(queued(&store, Phase::Review), [WorkSubject::Candidate(id)]);

        // Nor does it carry what a fresh verdict would refuse: a refuted member flaw.
        verdict(&store, &actor, id, Verdict::Validated);
        store
            .refute_flaw(&actor, flaw, &text("bounded upstream"))
            .unwrap();
        assess(&store, &actor, id);
        assert_eq!(status(&store, id), Status::Shortlisted);
    }

    #[test]
    fn material_changes_requeue_assessment_and_premises_gate_the_verdict() {
        let (_directory, store, actor) = fixture();
        let id = validated(&store, &actor);
        let (flaw, path) = member(&store, id);
        assert!(store.path_work(path).unwrap().decisions.is_empty());
        assert_eq!(queued(&store, Phase::Poc), [WorkSubject::Candidate(id)]);

        // Contradicting evidence on the flaw reopens every decision built on it.
        store
            .add_evidence(
                &actor,
                EvidenceSubject::Flaw(flaw),
                &contradiction("bounded upstream"),
            )
            .unwrap();
        assert_eq!(status(&store, id), Status::Identified);
        assert!(queued(&store, Phase::Poc).is_empty());
        assert_eq!(queued(&store, Phase::Assess), [WorkSubject::Candidate(id)]);
        let work = store.path_work(path).unwrap();
        assert_eq!(
            work.decisions,
            [flaw],
            "the path owes a decision at the flaw's new revision"
        );
        contributes(&store, &actor, flaw, path);
        assess(&store, &actor, id);
        assert!(store.path_work(path).unwrap().decisions.is_empty());

        // A scenario premise gates the verdict, not the assessment: a reviewer's premises
        // and their resolutions leave the assessment it reviews standing.
        let premise = store
            .add_premise(
                &actor,
                scenario(id),
                &new_premise(InvariantKind::Other, "the caller passes the tail"),
            )
            .unwrap();
        assert_eq!(status(&store, id), Status::Shortlisted);
        assert!(matches!(
            store.record_validation(&actor, id, Verdict::Validated, &text("premise open")),
            Err(Error::Refused(Refusal::OpenPremise { .. }))
        ));
        holds(&store, &actor, premise);
        verdict(&store, &actor, id, Verdict::Validated);
        assert_eq!(status(&store, id), Status::Validated);

        // Evidence against the premise reopens it: the verdict stands again only once it
        // holds again, with no new assessment owed on the way.
        let revision = store.candidate(id).unwrap().candidate.revision;
        store
            .add_evidence(
                &actor,
                EvidenceSubject::Premise(premise),
                &contradiction("one caller skips the check"),
            )
            .unwrap();
        assert_eq!(store.premise(premise).unwrap().evidence.len(), 1);
        assert_eq!(
            store.candidate(id).unwrap().premises[0]
                .premise
                .evidence
                .len(),
            1
        );
        assert_eq!(status(&store, id), Status::Shortlisted);
        assert!(queued(&store, Phase::Assess).is_empty());
        assert_eq!(queued(&store, Phase::Review), [WorkSubject::Candidate(id)]);
        holds(&store, &actor, premise);
        assert_eq!(status(&store, id), Status::Validated);
        assert_eq!(store.candidate(id).unwrap().candidate.revision, revision);
    }

    #[test]
    fn merges_move_work_to_the_canonical_record_and_reopen_it() {
        let (_directory, store, actor) = fixture();
        let (first, second) = (shortlisted(&store, &actor), shortlisted(&store, &actor));
        let second_flaw = store.candidate(second).unwrap().members[0].flaw.id;
        let first_path = store.candidate(first).unwrap().paths[0];
        let [kept, dropped] =
            ["the tail is attacker-sized", "the caller retries"].map(|statement| {
                store
                    .add_premise(
                        &actor,
                        scenario(second),
                        &new_premise(InvariantKind::Bounds, statement),
                    )
                    .unwrap()
            });
        let [settles_kept, settles_dropped] = [kept, dropped].map(|premise| {
            let work = follow_up_unit(Some(second), None, vec![Invariant::Premise(premise)]);
            store.plan_unit(&actor, &work).unwrap()
        });
        assert!(matches!(
            store.merge_candidates(
                &actor,
                second,
                first,
                &[kept, dropped, PremiseId(999)],
                &text("x")
            ),
            Err(Error::NotFound {
                record: Record::Premise,
                ..
            })
        ));
        store
            .merge_candidates(&actor, second, first, &[kept], &text("one root cause"))
            .unwrap();
        // A unit moves only with every premise it names; one asking about a dropped premise
        // stays with the duplicate and ends.
        let candidate_of = |unit| match store.unit(unit).unwrap().work {
            UnitWork::FollowUp { candidate, .. } => candidate,
            _ => unreachable!("a follow-up"),
        };
        assert_eq!(candidate_of(settles_kept), Some(first));
        assert!(store.unit(settles_kept).unwrap().outcome.is_none());
        assert_eq!(candidate_of(settles_dropped), Some(second));
        let ended = store.unit(settles_dropped).unwrap().outcome.unwrap();
        assert_eq!(ended.outcome, Outcome::Deferred);
        assert_eq!(ended.note, format!("merged into #{}", first.0));
        assert_eq!(status(&store, second), Status::Merged);
        assert_eq!(status(&store, first), Status::Identified);
        let merged = store.candidate(first).unwrap();
        assert_eq!(merged.members.len(), 2);
        assert_eq!(merged.paths.len(), 2);
        assert_eq!(merged.premises.len(), 1);
        assert_eq!(merged.premises[0].premise.id, kept);
        assert_eq!(
            store.candidate(second).unwrap().premises[0].premise.id,
            dropped
        );
        assert!(queued(&store, Phase::Review).is_empty());
        // The first path never decided the second flaw, so it owes that decision.
        let work = store.path_work(first_path).unwrap();
        assert_eq!(work.decisions, [second_flaw]);
        assert_eq!(
            queued(&store, Phase::Assess),
            [WorkSubject::Candidate(first)]
        );
        assert!(matches!(
            assess_at(&store, &actor, second, HIGH, Assessor::Researcher),
            Err(Error::Merged { .. })
        ));

        // A related duplicate merges, its relation moving to the survivor; a decision the
        // duplicate holds at a revision past the survivor's never stands for it.
        let first_flaw = merged.members[0].flaw.id;
        let third = store
            .add_flaw(
                &actor,
                &new_flaw(
                    store.flaw(first_flaw).unwrap().record.flaw.component,
                    "third",
                ),
            )
            .unwrap();
        store
            .relate_flaws(
                &actor,
                second_flaw,
                third,
                RelationKind::Enables,
                &text("feeds it"),
            )
            .unwrap();
        store
            .add_evidence(
                &actor,
                EvidenceSubject::Flaw(second_flaw),
                &contradiction("narrower than stated"),
            )
            .unwrap();
        let second_path = store.flaw(second_flaw).unwrap().exposures[0].path;
        contributes(&store, &actor, second_flaw, second_path);
        assert!(flaw_revision(&store, second_flaw) > flaw_revision(&store, first_flaw));
        store
            .merge_flaws(&actor, second_flaw, first_flaw, &text("same write"))
            .unwrap();
        let survivor = store.flaw(first_flaw).unwrap();
        assert_eq!(survivor.exposures.len(), 2);
        assert!(survivor.exposures.iter().all(|exposure| !exposure.current));
        assert_eq!(survivor.relations.len(), 1);
        assert_eq!(
            (survivor.relations[0].a, survivor.relations[0].b),
            (first_flaw, third)
        );
        assert_eq!(store.candidate(first).unwrap().members.len(), 1);
    }

    #[test]
    fn a_candidate_raised_before_tracing_owes_its_path_review() {
        let (_directory, store, actor) = fixture();
        let path = path(&store, &actor, "stream", &[]);
        let component = component(&store, &actor, "decoder");
        let flaw = FlawChoice::New(Box::new(new_flaw(component, "unbounded tail")));
        let id = (store.add_candidate(&actor, &new_candidate(flaw, vec![path]))).unwrap();
        let work = store.path_work(path).unwrap();
        assert_eq!(
            work.decisions,
            [store.candidate(id).unwrap().members[0].flaw.id]
        );
        assess(&store, &actor, id);
        assert!(matches!(
            store.record_validation(&actor, id, Verdict::Validated, &text("untraced")),
            Err(Error::Refused(Refusal::NoContribution { .. }))
        ));
    }

    /// A test is owed until one settles it, and a pending one waits on its blocker. A settled
    /// test is owed again only once what it rests on changed after the latest attempt, and
    /// never while nothing did: an unavailable one after a new environment, a failed one after
    /// either a new environment or a reassessment. A declined one stays declined.
    #[test]
    fn a_settled_test_is_owed_again_only_after_what_it_rests_on_changed() {
        let (_directory, store, actor) = fixture();
        let id = validated(&store, &actor);
        let (flaw, path) = member(&store, id);
        let owed = || queued(&store, Phase::Poc) == [WorkSubject::Candidate(id)];
        let test = |result| poc(&store, &actor, id, result);
        let environment = || environment(&store, &actor);
        let scope = || {
            store
                .add_scope(&actor, &text("tests approved"), &[])
                .unwrap()
        };
        assert!(owed());
        test(PocResult::Pending);
        assert!(!owed());
        assert_eq!(
            queued(&store, Phase::PocPending),
            [WorkSubject::Candidate(id)]
        );
        test(PocResult::Declined);
        assert!(queued(&store, Phase::PocPending).is_empty());
        assert_eq!(confidence(&store, id), Some(ConfidenceTier::Confirmed));
        environment();
        scope();
        assert!(!owed(), "a decline stands until the user asks again");

        test(PocResult::Unavailable);
        scope();
        assert!(
            !owed(),
            "a scope leaves what the environment could not run as it was"
        );
        environment();
        assert!(owed());
        store.set_poc_policy(PocPolicy::No).unwrap();
        assert!(!owed(), "nothing is owed while the launch declines testing");
        store.set_poc_policy(PocPolicy::Yes).unwrap();
        test(PocResult::Failed);
        assert!(!owed());
        // A material change whose reassessment carries the verdict and the failed test.
        store
            .add_evidence(
                &actor,
                EvidenceSubject::Flaw(flaw),
                &contradiction("narrower"),
            )
            .unwrap();
        contributes(&store, &actor, flaw, path);
        assess(&store, &actor, id);
        assert_eq!(status(&store, id), Status::Validated);
        assert!(owed());
        test(PocResult::Failed);
        environment();
        assert!(owed());
        test(PocResult::Unavailable);
        assert!(
            !owed(),
            "the failure stands, and nothing changed since the latest attempt"
        );
        assert_eq!(
            snapshot(&store, root_of(&store, id))
                .content
                .headline
                .poc
                .map(|poc| poc.result),
            Some(PocResult::Failed)
        );
    }

    /// A report waits while the candidate owes a test, and is owed again once a later test
    /// changes what it would say. A failed test is reported, at `confirmed`, and stands until
    /// a confirmation: neither a later untested outcome nor a renewed verdict resolves it.
    #[test]
    fn report_work_follows_poc_outcomes_until_confirmation() {
        let (_directory, store, actor) = fixture();
        let id = validated(&store, &actor);
        let root = root_of(&store, id);
        assert!(queued(&store, Phase::Report).is_empty());
        assert_eq!(store.synthesis().unwrap().owed(Phase::Report), 1);
        let test = |result| poc(&store, &actor, id, result);
        test(PocResult::Unavailable);
        assert_eq!(queued(&store, Phase::Report), [WorkSubject::Root(root)]);
        // A retry the user asks for is recorded whatever the queue owes.
        test(PocResult::Failed);
        test(PocResult::Unavailable);
        verdict(&store, &actor, id, Verdict::Validated);
        assert_eq!(queued(&store, Phase::Report), [WorkSubject::Root(root)]);
        let headline = snapshot(&store, root).content.headline;
        assert_eq!(
            (
                headline.candidate.candidate.confidence,
                headline.poc.map(|poc| poc.result)
            ),
            (Some(ConfidenceTier::Confirmed), Some(PocResult::Failed))
        );
        test(PocResult::Confirmed);
        assert_eq!(queued(&store, Phase::Report), [WorkSubject::Root(root)]);
        assert_eq!(confidence(&store, id), Some(ConfidenceTier::Demonstrated));
    }

    /// A candidate's assessment is queued alone, and waits while a review is open of any path
    /// reaching a member's component, though the path is not the candidate's own.
    #[test]
    fn an_assessment_waits_for_reviews_of_paths_reaching_its_members() {
        let (_directory, store, actor) = fixture();
        let decoder = component(&store, &actor, "decoder");
        let own = path(&store, &actor, "stream", &[decoder]);
        reviewed(&store, &actor);
        let flaw = store.add_flaw(&actor, &new_flaw(decoder, "tail")).unwrap();
        let id = store
            .add_candidate(
                &actor,
                &new_candidate(FlawChoice::Existing(flaw), vec![own]),
            )
            .unwrap();
        let assess = || queued(&store, Phase::Assess);
        assert_eq!(assess(), [WorkSubject::Candidate(id)]);
        path(&store, &actor, "upload", &[decoder]);
        assert!(assess().is_empty());
        assert_eq!(store.synthesis().unwrap().owed(Phase::Assess), 1);
        reviewed(&store, &actor);
        assert_eq!(assess(), [WorkSubject::Candidate(id)]);
    }

    /// Fresh work stays owed whatever ran before it: after one candidate's review and test,
    /// the alternative a rejecting reviewer raises owes its assessment, its review and then its
    /// test.
    #[test]
    fn a_reviews_alternative_owes_fresh_work_after_earlier_reviews_and_tests() {
        let (_directory, store, actor) = fixture();
        let tested = validated(&store, &actor);
        poc(&store, &actor, tested, PocResult::Confirmed);
        let rejected = shortlisted(&store, &actor);
        verdict(&store, &actor, rejected, Verdict::Disproved);
        let (flaw, path) = member(&store, rejected);
        let alternative = store
            .add_candidate(
                &actor,
                &new_candidate(FlawChoice::Existing(flaw), vec![path]),
            )
            .unwrap();
        let owed = |phase| queued(&store, phase) == [WorkSubject::Candidate(alternative)];
        assert!(owed(Phase::Assess));
        assess(&store, &actor, alternative);
        assert!(owed(Phase::Review));
        verdict(&store, &actor, alternative, Verdict::Validated);
        assert!(owed(Phase::Poc));
    }

    #[test]
    fn a_published_untested_verdict_still_owes_its_first_test() {
        let (_directory, store, actor) = fixture();
        store.set_poc_policy(PocPolicy::No).unwrap();
        let id = validated(&store, &actor);
        let version = store.report_generation(root_of(&store, id)).unwrap().key;
        store
            .record_finding(&actor, &report_input(root_of(&store, id)), &version, &[])
            .unwrap();
        assert_eq!(status(&store, id), Status::Reported);
        store.set_poc_policy(PocPolicy::Yes).unwrap();
        assert_eq!(queued(&store, Phase::Poc), [WorkSubject::Candidate(id)]);
        poc(&store, &actor, id, PocResult::Failed);
        assert_eq!(status(&store, id), Status::Validated);
    }

    /// A root's report entry rests on its generation, the key its finding is recorded under:
    /// a reassessment its verdict carries to owes a report on a new basis, though the verdict
    /// and test stay the same.
    #[test]
    fn a_report_rests_on_the_generation_it_publishes() {
        let (_directory, store, actor) = fixture();
        store.set_poc_policy(PocPolicy::No).unwrap();
        let id = validated(&store, &actor);
        let root = root_of(&store, id);
        let basis = || {
            (entries(&store, Phase::Report))
                .into_iter()
                .find(|entry| entry.subject == WorkSubject::Root(root))
                .and_then(|entry| entry.basis)
        };
        let published = store.report_generation(root).unwrap().key;
        assert_eq!(
            basis(),
            Some(Basis::Report {
                generation: published
            })
        );
        store
            .record_finding(&actor, &report_input(root), &published, &[])
            .unwrap();
        assert_eq!(basis(), None);
        let other = path(&store, &actor, "second entry", &[]);
        store.add_paths(&actor, id, &[other]).unwrap();
        assess(&store, &actor, id);
        assert_eq!(status(&store, id), Status::Validated);
        let renewed = store.report_generation(root).unwrap().key;
        assert_ne!(renewed, published);
        assert_eq!(
            basis(),
            Some(Basis::Report {
                generation: renewed
            })
        );
    }

    /// An unproven verdict needs something to wait on, and each open condition gating the
    /// scenario, one added later included, gets a follow-up the store plans, which its
    /// reviewer may answer; a reopened condition gets a fresh one. Its queue entry is about
    /// the candidate and the condition. Once every follow-up naming the candidate ended, the
    /// candidate is owed review again, its entry about the candidate, its member flaws
    /// and its premises, though a condition is still open: its follow-up ended at its
    /// revision, so nothing more is planned and a further unproven verdict is refused until a
    /// change reopens it.
    #[test]
    fn an_unproven_verdict_waits_on_its_pending_work_then_returns_to_review() {
        let (_directory, store, actor) = fixture();
        let id = shortlisted(&store, &actor);
        let reviewer = answerer(&actor);
        let unproven = || store.record_validation(&reviewer, id, Verdict::Unproven, &text("open"));
        assert!(matches!(unproven(), Err(Error::NothingPending { .. })));
        let condition = |statement| {
            store
                .add_premise(
                    &reviewer,
                    scenario(id),
                    &new_premise(InvariantKind::State, statement),
                )
                .unwrap()
        };
        let first = condition("the stream resumes mid-block");
        unproven().unwrap();
        let second = condition("the window is not yet full");
        store
            .add_evidence(
                &actor,
                EvidenceSubject::Premise(first),
                &contradiction("a reset clears it"),
            )
            .unwrap();
        let follow_ups = || {
            (units(&store, UnitKind::FollowUp).into_iter())
                .map(|unit| match unit.work {
                    UnitWork::FollowUp {
                        candidate,
                        flaw: None,
                        ref invariants,
                        ..
                    } if candidate == Some(id) && unit.planned_by == actor => {
                        let [Invariant::Premise(premise)] = invariants[..] else {
                            panic!("one condition per follow-up: {invariants:?}");
                        };
                        (unit.id, premise)
                    }
                    work => panic!("not a store-planned premise follow-up: {work:?}"),
                })
                .collect::<Vec<_>>()
        };
        let [(settle_first, a), (settle_second, b), (reopened, c)] = follow_ups()[..] else {
            panic!(
                "a follow-up per condition and reopening: {:?}",
                follow_ups()
            );
        };
        assert_eq!((a, b, c), (first, second, first));
        assert_eq!(
            entries(&store, Phase::FollowUp)[0].about,
            [About::Candidate(id), About::Premise(first)]
        );

        let review = || queued(&store, Phase::Review);
        holds(&store, &reviewer, first);
        for unit in [settle_first, reopened] {
            store
                .record_outcome(&reviewer, unit, Outcome::Completed, &text("holds"), &[])
                .unwrap();
        }
        assert!(review().is_empty(), "a follow-up naming it is still open");
        store
            .record_outcome(
                &reviewer,
                settle_second,
                Outcome::Inconclusive,
                &text("needs a build"),
                &[OpenPremise {
                    premise: second,
                    untraced: Vec::new(),
                }],
            )
            .unwrap();
        assert_eq!(status(&store, id), Status::Unproven);
        assert!(matches!(unproven(), Err(Error::NothingPending { .. })));
        assert_eq!(follow_ups().len(), 3);
        let [entry] = &entries(&store, Phase::Review)[..] else {
            panic!("one review owed")
        };
        assert_eq!(entry.subject, WorkSubject::Candidate(id));
        assert_eq!(
            entry.about,
            [
                About::Candidate(id),
                About::Flaw(member(&store, id).0),
                About::Premise(first),
                About::Premise(second)
            ]
        );
        store
            .add_evidence(
                &actor,
                EvidenceSubject::Premise(second),
                &contradiction("the window refills on reset"),
            )
            .unwrap();
        assert_eq!(
            follow_ups().len(),
            4,
            "a reopened condition is followed up again"
        );
        assert!(review().is_empty());
    }

    /// A returning unproven review decides each condition its follow-up left open: by the
    /// scope's facts, or by asking the user, which holds the review back until a new scope
    /// answers and the condition is decided by it.
    #[test]
    fn an_exhausted_condition_is_decided_by_the_scope_or_asked_of_the_user() {
        let (_directory, store, actor) = fixture();
        let fact = |statement| NewDeployment {
            kind: DeploymentKind::HostBehaviour,
            stance: DeploymentStance::Admitted,
            statement: text(statement),
        };
        let scopes = || Listing::drain(|page| store.list_scopes(page)).unwrap();
        store
            .add_scope(
                &actor,
                &text("embedding hosts"),
                &[fact("hosts relay streams")],
            )
            .unwrap();
        let id = shortlisted(&store, &actor);
        let reviewer = answerer(&actor);
        let statement = "a remote party selects the window size";
        let premise = store
            .add_premise(
                &reviewer,
                scenario(id),
                &new_premise(InvariantKind::Authorization, statement),
            )
            .unwrap();
        let verdict = |verdict| store.record_validation(&reviewer, id, verdict, &text("review"));
        verdict(Verdict::Unproven).unwrap();
        let [WorkSubject::Unit(settle)] = queued(&store, Phase::FollowUp)[..] else {
            panic!("one settle follow-up")
        };
        store
            .record_outcome(
                &reviewer,
                settle,
                Outcome::Inconclusive,
                &text("host-defined"),
                &[OpenPremise {
                    premise,
                    untraced: Vec::new(),
                }],
            )
            .unwrap();
        let review = || queued(&store, Phase::Review);
        assert_eq!(review(), [WorkSubject::Candidate(id)]);
        assert!(matches!(
            verdict(Verdict::Unproven),
            Err(Error::NothingPending { .. })
        ));

        let question = "May a remote party choose the window size?";
        let resolve =
            |resolution| store.resolve_premise(&reviewer, premise, resolution, &text(question));
        resolve(Resolution::Asked).unwrap();
        assert!(matches!(
            verdict(Verdict::Validated),
            Err(Error::Refused(Refusal::OpenPremise { .. }))
        ));
        verdict(Verdict::Unproven).unwrap();
        assert!(review().is_empty(), "the review waits on the user's answer");
        assert_eq!(
            scopes().pop().unwrap().asked,
            [AskedCondition {
                premise,
                candidate: id,
                statement: statement.to_owned(),
                question: question.to_owned(),
            }]
        );

        store
            .add_scope(
                &actor,
                &text("embedding hosts, answered"),
                &[fact("hosts let remote parties choose the window size")],
            )
            .unwrap();
        let answered = scopes().pop().unwrap();
        assert!(answered.asked.is_empty());
        assert_eq!(review(), [WorkSubject::Candidate(id)]);
        assert!(matches!(
            verdict(Verdict::Unproven),
            Err(Error::NothingPending { .. })
        ));
        resolve(Resolution::Holds {
            deployment: Some(answered.deployment[0].id),
        })
        .unwrap();
        verdict(Verdict::Validated).unwrap();
        assert_eq!(status(&store, id), Status::Validated);
    }

    /// A follow-up about a candidate holds its tests while it is open, though they stay
    /// owed; it names only a live candidate, never one its planner validated, and a merge
    /// moves it.
    #[test]
    fn a_follow_up_about_a_candidate_holds_its_tests() {
        let (_directory, store, actor) = fixture();
        let id = validated(&store, &actor);
        let ask = |actor: &AgentId, candidate| {
            store.plan_unit(actor, &follow_up_unit(Some(candidate), None, Vec::new()))
        };
        assert!(matches!(
            ask(&actor, id),
            Err(Error::QuestionsOwnVerdict { .. })
        ));
        let other = child_agent(&store, JobState::Running).0;
        let unit = ask(&other, id).unwrap();
        assert!(queued(&store, Phase::Poc).is_empty());
        assert_eq!(store.synthesis().unwrap().owed(Phase::Poc), 1);

        let survivor = shortlisted(&store, &actor);
        store
            .merge_candidates(&actor, id, survivor, &[], &text("one scenario"))
            .unwrap();
        assert!(matches!(
            store.unit(unit).unwrap().work,
            UnitWork::FollowUp { candidate: Some(candidate), .. } if candidate == survivor
        ));
        assert!(ask(&other, id).is_err());
    }

    /// A condition gating one metric does not block a verdict: while it does not hold, the
    /// verdict needs that metric scored at its least severe value, and stands without it.
    /// One that stops holding after the verdict stops it standing until the metric is
    /// re-scored at its floor and reviewed; the test stands throughout.
    #[test]
    fn an_open_metric_condition_scores_its_metric_at_the_floor() {
        let (_directory, store, actor) = fixture();
        let id = shortlisted(&store, &actor);
        let gates = Gate::Metric {
            metric: crate::cvss::BaseMetric::AttackRequirements,
        };
        let premise = store
            .add_premise(
                &actor,
                PremiseOn::Candidate { id, gates },
                &new_premise(
                    InvariantKind::State,
                    "the build enables the optional decoder",
                ),
            )
            .unwrap();
        let validate =
            || store.record_validation(&actor, id, Verdict::Validated, &text("reviewed"));
        assert!(matches!(
            validate(),
            Err(Error::Refused(Refusal::MetricAboveFloor { .. }))
        ));
        holds(&store, &actor, premise);
        validate().unwrap();
        poc(&store, &actor, id, PocResult::Confirmed);
        store
            .add_evidence(
                &actor,
                EvidenceSubject::Premise(premise),
                &contradiction("the decoder is optional"),
            )
            .unwrap();
        assert_eq!(status(&store, id), Status::Shortlisted);
        assert_eq!(queued(&store, Phase::Review), [WorkSubject::Candidate(id)]);
        let floor = "CVSS:4.0/AV:N/AC:L/AT:P/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N";
        assess_at(&store, &actor, id, floor, Assessor::Reviewer).unwrap();
        assert_eq!(status(&store, id), Status::Shortlisted);
        validate().unwrap();
        assert_eq!(confidence(&store, id), Some(ConfidenceTier::Demonstrated));
    }

    /// Candidates whose flaws trace back through `enables` to one root form its group: the
    /// group's most severe untested scenario is tested first, the next one once that test
    /// fails, a scenario no more severe than a demonstrated one is noted rather than tested,
    /// a more severe one still owes its own test, and the root is reported once, headed by
    /// the demonstrated scenario.
    #[test]
    fn a_root_group_is_tested_from_its_most_severe_scenario_and_reported_once() {
        let (_directory, store, actor) = fixture();
        const LOCAL: &str = "CVSS:4.0/AV:L/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N";
        let validated_at = |name: &str, scored: &str| {
            let (id, flaw, _) = candidate(&store, &actor, name);
            assess_at(&store, &actor, id, scored, Assessor::Researcher).unwrap();
            verdict(&store, &actor, id, Verdict::Validated);
            (id, flaw)
        };
        let confirm = |id| poc(&store, &actor, id, PocResult::Confirmed);
        let tests = || queued(&store, Phase::Poc);
        let (low, root) = validated_at("tail", LOCAL);
        assert_eq!(tests(), [WorkSubject::Candidate(low)]);
        confirm(low);
        let (same, same_flaw) = validated_at("copy", LOCAL);
        let (worse, worse_flaw) = validated_at("wrap", HIGH);
        let (later, later_flaw) = validated_at("dict", HIGH);
        for consumer in [same_flaw, worse_flaw, later_flaw] {
            store
                .relate_flaws(
                    &actor,
                    root,
                    consumer,
                    RelationKind::Enables,
                    &text("feeds it"),
                )
                .unwrap();
        }
        assert_eq!(store.candidate(worse).unwrap().roots, [root]);
        let group = CandidateFilter {
            root: Some(root),
            ..CandidateFilter::default()
        };
        assert_eq!(
            (store.candidates(&group, Page::default()).unwrap().iter())
                .map(|candidate| candidate.id)
                .collect::<Vec<_>>(),
            [low, same, worse, later]
        );
        assert_eq!(status(&store, same), Status::Noted);
        assert_eq!(
            tests(),
            [WorkSubject::Candidate(worse)],
            "the oldest most severe first"
        );
        assert_eq!(store.synthesis().unwrap().owed(Phase::Poc), 2);
        assert!(queued(&store, Phase::Report).is_empty());

        // A failed head lets the next most severe scenario through, and the demonstration
        // notes the failed one.
        poc(&store, &actor, worse, PocResult::Failed);
        assert_eq!(tests(), [WorkSubject::Candidate(later)]);
        confirm(later);
        assert_eq!(status(&store, worse), Status::Noted);
        assert_eq!(queued(&store, Phase::Report), [WorkSubject::Root(root)]);
        let snapshot = snapshot(&store, root);
        let content = &snapshot.content;
        assert_eq!(content.headline.candidate.candidate.id, later);
        assert_eq!(
            (content.variants.iter())
                .map(|variant| (
                    variant.candidate.candidate.id,
                    variant.candidate.candidate.status,
                    variant.candidate.candidate.confidence
                ))
                .collect::<Vec<_>>(),
            [
                (low, Status::Validated, Some(ConfidenceTier::Demonstrated)),
                (worse, Status::Noted, Some(ConfidenceTier::Confirmed)),
                (same, Status::Noted, Some(ConfidenceTier::Confirmed))
            ]
        );
        let finding = store
            .record_finding(&actor, &report_input(root), &snapshot.key, &[])
            .unwrap();
        assert_eq!(
            (finding.key, finding.severity),
            (snapshot.key, crate::cvss::Severity::Critical)
        );
        assert_eq!(finding.candidates, [later, low, same, worse]);
        assert_eq!(status(&store, same), Status::Reported);
        assert!(queued(&store, Phase::Report).is_empty());
        let again = store.record_finding(&actor, &report_input(root), &snapshot.key, &[]);
        assert_eq!(
            again.unwrap().id,
            finding.id,
            "the same scenarios publish it again"
        );
    }

    /// A finding is stale once its scenarios differ from the group's reportable ones either
    /// way: a scenario that left owes the root a new report, which supersedes it, and once
    /// none is left the latest finding stands withdrawn, owing nothing.
    #[test]
    fn a_scenario_leaving_its_finding_owes_a_new_report_or_withdraws_it() {
        let (_directory, store, actor) = fixture();
        store.set_poc_policy(PocPolicy::No).unwrap();
        let (kept, left) = (validated(&store, &actor), validated(&store, &actor));
        let root = root_of(&store, kept);
        store
            .relate_flaws(
                &actor,
                root,
                member(&store, left).0,
                RelationKind::Enables,
                &text("feeds it"),
            )
            .unwrap();
        let publish = || {
            let key = store.report_generation(root).unwrap().key;
            store
                .record_finding(&actor, &report_input(root), &key, &[])
                .unwrap()
        };
        let statuses = || {
            (store.findings(None, Page::default()).unwrap().items.iter())
                .map(|finding| finding.status)
                .collect::<Vec<_>>()
        };
        let both = publish();
        assert_eq!(both.candidates, [kept, left]);
        let disprove = |id| verdict(&store, &actor, id, Verdict::Disproved);
        disprove(left);
        assert_eq!(statuses(), [FindingStatus::Stale]);
        assert_eq!(queued(&store, Phase::Report), [WorkSubject::Root(root)]);
        let kept_only = publish();
        assert_eq!(kept_only.candidates, [kept]);
        assert!(queued(&store, Phase::Report).is_empty());

        disprove(kept);
        assert!(queued(&store, Phase::Report).is_empty());
        assert_eq!(store.synthesis().unwrap().owed(Phase::Report), 0);
        assert_eq!(
            statuses(),
            [
                FindingStatus::Superseded { by: kept_only.id },
                FindingStatus::Withdrawn
            ]
        );
    }

    /// A demonstration notes its group's scenarios only while the report states it: once it
    /// is disproved, or merged into its untested sibling, the sibling owes its own test.
    #[test]
    fn a_demonstration_the_report_drops_notes_no_sibling() {
        fn disprove(store: &Store, actor: &AgentId, demonstrated: CandidateId, _: CandidateId) {
            verdict(store, actor, demonstrated, Verdict::Disproved);
        }
        fn merge(store: &Store, actor: &AgentId, demonstrated: CandidateId, sibling: CandidateId) {
            store
                .merge_candidates(actor, demonstrated, sibling, &[], &text("one scenario"))
                .unwrap();
            assess(store, actor, sibling);
        }
        for leave in [
            disprove as fn(&Store, &AgentId, CandidateId, CandidateId),
            merge,
        ] {
            let (_directory, store, actor) = fixture();
            let demonstrated = validated(&store, &actor);
            poc(&store, &actor, demonstrated, PocResult::Confirmed);
            let sibling = validated(&store, &actor);
            store
                .relate_flaws(
                    &actor,
                    root_of(&store, demonstrated),
                    member(&store, sibling).0,
                    RelationKind::Enables,
                    &text("feeds it"),
                )
                .unwrap();
            assert_eq!(status(&store, sibling), Status::Noted);
            leave(&store, &actor, demonstrated, sibling);
            assert_eq!(status(&store, sibling), Status::Validated);
            assert_eq!(
                queued(&store, Phase::Poc),
                [WorkSubject::Candidate(sibling)]
            );
        }
    }
}

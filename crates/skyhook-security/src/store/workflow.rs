//! Verdicts and empirical tests: append-only, each bound to what it judged.

use chrono::{DateTime, Utc};
use libsql::Row;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook::db::{Db, enum_column, params};

use super::*;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ValidationAttempt {
    pub id: ValidationId,
    pub candidate: CandidateId,
    /// The assessment it judged.
    pub assessment: AssessmentId,
    pub verdict: Verdict,
    pub evidence: String,
    pub actor: AgentId,
    pub at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PocAttempt {
    pub id: PocId,
    pub candidate: CandidateId,
    /// The verdict it tested. The attempt stands until a contradiction of the scenario: a
    /// member refuted, or a condition gating it failing or undermined.
    pub validation: ValidationId,
    /// The opened test it settles, if one was opened.
    pub start: Option<PocStartId>,
    pub result: PocResult,
    pub evidence: String,
    pub artifacts: Vec<PocArtifact>,
    pub actor: AgentId,
    pub at: DateTime<Utc>,
}

/// Validations read as `v`, decoded by [`validation_row`]; a condition follows.
macro_rules! validation_select {
    () => {
        concat!(
            "SELECT v.id,v.candidate,v.assessment,v.verdict,v.reason,v.evidence,v.at,v.deployment,",
            actor_columns!(),
            " FROM validation v JOIN actor a ON a.id=v.actor WHERE "
        )
    };
}
/// PoC attempts read as `p`, decoded by [`poc_row`]; a condition follows.
macro_rules! poc_select {
    () => {
        concat!(
            "SELECT p.id,p.candidate,p.validation,p.start,p.result,p.evidence,p.at,",
            actor_columns!(),
            " FROM poc p JOIN actor a ON a.id=p.actor WHERE "
        )
    };
}

fn validation_row(row: &Row) -> Result<ValidationAttempt> {
    Ok(ValidationAttempt {
        id: ValidationId(row.get(0)?),
        candidate: CandidateId(row.get(1)?),
        assessment: AssessmentId(row.get(2)?),
        verdict: match enum_column(row, 3)? {
            VerdictKind::Validated => Verdict::Validated,
            VerdictKind::Unproven => Verdict::Unproven,
            VerdictKind::Rejected => match enum_column(row, 4)? {
                RejectionReason::Disproved => Verdict::Disproved,
                RejectionReason::PolicyExcluded => Verdict::PolicyExcluded {
                    deployment: row.get::<Option<i64>>(7)?.map(DeploymentId),
                },
            },
        },
        evidence: row.get(5)?,
        at: time(row.get(6)?)?,
        actor: actor_column(row, 8)?,
    })
}

fn poc_row(row: &Row) -> Result<PocAttempt> {
    Ok(PocAttempt {
        id: PocId(row.get(0)?),
        candidate: CandidateId(row.get(1)?),
        validation: ValidationId(row.get(2)?),
        start: row.get::<Option<i64>>(3)?.map(PocStartId),
        result: enum_column(row, 4)?,
        evidence: row.get(5)?,
        artifacts: Vec::new(),
        at: time(row.get(6)?)?,
        actor: actor_column(row, 7)?,
    })
}

fn with_artifacts(db: &Db, mut attempts: Vec<PocAttempt>) -> Result<Vec<PocAttempt>> {
    for attempt in &mut attempts {
        attempt.artifacts = rows(
            db,
            "SELECT reference,role FROM poc_artifact WHERE poc=?1 ORDER BY position",
            params![attempt.id.0],
            |row| {
                Ok(PocArtifact {
                    reference: row.get::<String>(0)?.parse()?,
                    role: enum_column(row, 1)?,
                })
            },
        )?;
    }
    Ok(attempts)
}

fn pocs(db: &Db, sql: &'static str, params: Vec<libsql::Value>) -> Result<Vec<PocAttempt>> {
    with_artifacts(db, rows(db, sql, params, poc_row)?)
}

/// A candidate's latest records of each kind, whether or not they stand.
#[derive(Clone, Debug)]
pub struct Latest {
    pub verdict: Option<Verdict>,
    pub poc: Option<PocResult>,
    pub finding: Option<Finding>,
}

/// A candidate's standing: the records its status is derived from.
pub(super) struct Standing {
    pub status: Status,
    pub assessment: Option<AssessmentId>,
    pub validation: Option<ValidationId>,
}

pub(super) fn standing(db: &Db, id: CandidateId) -> Result<Standing> {
    one(
        db,
        "SELECT status,assessment,validation FROM candidate_standing WHERE candidate=?1",
        id,
        |row| {
            Ok(Standing {
                status: enum_column(row, 0)?,
                assessment: row.get::<Option<i64>>(1)?.map(AssessmentId),
                validation: row.get::<Option<i64>>(2)?.map(ValidationId),
            })
        },
    )
}

impl Standing {
    /// The positive verdict on the current assessment.
    pub(super) fn positive(&self) -> Result<ValidationId> {
        match (self.status, self.validation) {
            (Status::Validated | Status::Noted | Status::Reported, Some(validation)) => {
                Ok(validation)
            }
            _ => Err(Error::NoPositiveValidation),
        }
    }
}

/// Refuse a test of the live candidate `id` against `tested` unless that is its positive
/// verdict, which still stands unless `start`, a test the actor opened, settles here.
fn testable(
    db: &Db,
    id: CandidateId,
    tested: ValidationId,
    start: Option<PocStartId>,
) -> Result<()> {
    candidates::live(db, id)?;
    positive_verdict(db, id, tested)?;
    if start.is_none() {
        standing(db, id)?.positive()?;
    }
    Ok(())
}

pub(super) fn validation(db: &Db, id: ValidationId) -> Result<ValidationAttempt> {
    one(
        db,
        concat!(validation_select!(), "v.id=?1"),
        id,
        validation_row,
    )
}

pub(super) fn poc(db: &Db, id: PocId) -> Result<PocAttempt> {
    (pocs(db, concat!(poc_select!(), "p.id=?1"), params![id.0])?.pop()).ok_or(Error::NotFound {
        record: Record::Poc,
        id: Some(id.0),
    })
}

/// A test some agent opened that nothing settled since, and whether that agent's job is
/// still live; a test whose job ended is abandoned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenTest {
    pub start: PocStartId,
    pub candidate: CandidateId,
    pub validation: ValidationId,
    pub live: bool,
}

/// Refuse a test of `tested` unless it is a positive verdict on the candidate.
fn positive_verdict(db: &Db, id: CandidateId, tested: ValidationId) -> Result<()> {
    if exists(
        db,
        "SELECT 1 FROM validation WHERE id=?1 AND candidate=?2 AND verdict='validated'",
        params![tested.0, id.0],
    )? {
        Ok(())
    } else {
        Err(Error::NotPositive { tested })
    }
}

/// Whether start `s` is open: the candidate has no test past the one `s` followed.
macro_rules! open_test {
    () => {
        "NOT EXISTS (SELECT 1 FROM poc p WHERE p.candidate=s.candidate AND p.id>ifnull(s.since,0))"
    };
}
pub(super) use open_test;

/// `actor`'s open test of `id`, if it opened one nothing settled since, refused once a
/// contradiction moved the candidate past the scenario it opened the test on.
fn open_start(db: &Db, actor: i64, id: CandidateId) -> Result<Option<PocStartId>> {
    let open = optional(
        db,
        concat!(
            "SELECT s.id,s.contradictions=c.contradictions FROM poc_start s JOIN candidate c ON \
             c.id=s.candidate WHERE s.candidate=?1 AND s.actor=?2 AND ",
            open_test!(),
            " ORDER BY s.id DESC LIMIT 1"
        ),
        params![id.0, actor],
        |row| Ok((PocStartId(row.get(0)?), row.get::<i64>(1)? != 0)),
    )?;
    match open {
        Some((_, false)) => Err(Error::Contradicted),
        open => Ok(open.map(|(start, _)| start)),
    }
}

/// Refuse a positive verdict on the candidate's current assessment while `verdict_refusal`
/// lists anything for it.
fn gate(db: &Db, id: CandidateId) -> Result<()> {
    let refusal = optional(
        db,
        "SELECT refusal,flaw,premise,metric FROM verdict_refusal WHERE candidate=?1 ORDER BY \
         rank,flaw,premise LIMIT 1",
        params![id.0],
        |row| {
            let flaw = || Ok::<_, Error>(FlawId(row.get(1)?));
            let premise = || Ok::<_, Error>(PremiseId(row.get(2)?));
            Ok(match enum_column(row, 0)? {
                RefusalKind::NoPath => Refusal::NoPath { candidate: id },
                RefusalKind::Refuted => Refusal::Refuted { flaw: flaw()? },
                RefusalKind::NoContribution => Refusal::NoContribution {
                    candidate: id,
                    flaw: flaw()?,
                },
                RefusalKind::MetricAboveFloor => Refusal::MetricAboveFloor {
                    premise: premise()?,
                    metric: enum_column(row, 3)?,
                },
                RefusalKind::OpenPremise => Refusal::OpenPremise {
                    premise: premise()?,
                },
                RefusalKind::FailedPremise => Refusal::FailedPremise {
                    premise: premise()?,
                },
            })
        },
    )?;
    refusal.map_or(Ok(()), |refusal| Err(refusal.into()))
}

impl Store {
    /// A verdict on the candidate's current assessment; a later one on the same
    /// assessment supersedes it. A positive verdict needs what [`gate`] checks. An unproven
    /// one plans a follow-up settling each open condition gating its scenario that none was
    /// planned for at its revision, and is refused unless it then waits on something.
    pub fn record_validation(
        &self,
        actor: &AgentId,
        id: CandidateId,
        verdict: Verdict,
        evidence: &Text,
    ) -> Result<ValidationAttempt> {
        self.write(|db| {
            candidates::live(db, id)?;
            let standing = standing(db, id)?;
            let assessment = standing
                .assessment
                .ok_or(Error::Unassessed(standing.status))?;
            if let Some(deployment) = verdict.deployment() {
                scope::cite(db, deployment, DeploymentStance::Excluded)?;
            }
            if verdict == Verdict::Validated {
                gate(db, id)?;
            }
            let scope = scope::policy(db)?;
            let actor_id = actor_row(db, actor)?;
            let (kind, reason) = verdict.stored();
            let validation = ValidationId(db.insert(
                "INSERT INTO validation(candidate,assessment,verdict,reason,scope,deployment,evidence,actor,at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![id.0, assessment.0, kind, reason, scope.map(|id| id.0), verdict.deployment().map(|id| id.0), evidence, actor_id, now()],
            )?);
            audit(db, actor, EventKind::Validated, Detail::Record(EventRecord::Validation(validation)), &[Subject::Candidate(id)])?;
            premises::settle_owed(db, actor, id, None)?;
            if verdict == Verdict::Unproven
                && !exists(
                    db,
                    concat!("SELECT 1 FROM candidate c WHERE c.id=?1 AND ", queues::waits!()),
                    params![id.0],
                )?
            {
                return Err(Error::NothingPending { candidate: id });
            }
            self::validation(db, validation)
        })
    }

    /// Opens a test of the candidate against `tested`, a positive verdict on it, while its
    /// verdict stands, before any work, recording the job that runs it. The candidate shows
    /// as proving while the test is open and that job is live, and as abandoned once the job
    /// ends unsettled; neither changes what the candidate owes.
    pub fn start_poc(
        &self,
        actor: &AgentId,
        id: CandidateId,
        tested: ValidationId,
    ) -> Result<PocStartId> {
        self.write(|db| {
            testable(db, id, tested, None)?;
            let actor_id = actor_row(db, actor)?;
            let job = optional(
                db,
                "SELECT g.owner_job FROM actor a JOIN agent g ON g.id=a.session_agent WHERE \
                 a.id=?1 AND g.owner_job IS NOT NULL",
                params![actor_id],
                |row| Ok(row.get::<i64>(0)?),
            )?
            .ok_or(Error::TestNeedsJob)?;
            let start = PocStartId(db.insert(
                "INSERT INTO poc_start(candidate,validation,since,contradictions,actor,job,at) VALUES (?1,?2,(SELECT max(id) FROM poc WHERE candidate=?1),(SELECT contradictions FROM candidate WHERE id=?1),?3,?4,?5)",
                params![id.0, tested.0, actor_id, job, now()],
            )?);
            audit(db, actor, EventKind::PocStarted, Detail::Record(EventRecord::PocStart(start)), &[Subject::Candidate(id)])?;
            Ok(start)
        })
    }

    /// Tests open now, with whether each one's job is live. Read on every refresh: a job
    /// ending is no research write, so nothing cached by the research generation shows it.
    pub fn open_tests(&self) -> Result<Vec<OpenTest>> {
        self.read(|db| {
            rows(
                db,
                concat!(
                    "SELECT s.id,s.candidate,s.validation,EXISTS (SELECT 1 FROM job_status j JOIN \
                     job_state l ON l.name=j.state WHERE j.job=s.job AND NOT l.terminal) FROM \
                     poc_start s WHERE s.id=(SELECT max(id) FROM poc_start WHERE \
                     candidate=s.candidate) AND ",
                    open_test!(),
                    " ORDER BY s.candidate"
                ),
                Vec::new(),
                |row| {
                    Ok(OpenTest {
                        start: PocStartId(row.get(0)?),
                        candidate: CandidateId(row.get(1)?),
                        validation: ValidationId(row.get(2)?),
                        live: row.get::<i64>(3)? != 0,
                    })
                },
            )
        })
    }

    /// Attempts name the positive verdict they tested and stand until the scenario is
    /// contradicted: a material change, a renewed verdict or a re-scoring keeps them. An
    /// attempt that settles a test its actor opened is recorded whatever happened to the
    /// verdict since, unless a contradiction did; one without an open test needs a
    /// positive verdict now. A failed attempt stands until a later confirmation, which needs
    /// an attack and a control artifact from the same build, while the verdict stays
    /// reportable. A pending attempt waits on the user or the environment and settles
    /// nothing; an unavailable or declined one settles the test as untested.
    pub fn record_poc(
        &self,
        actor: &AgentId,
        id: CandidateId,
        tested: ValidationId,
        result: PocResult,
        evidence: &Text,
        artifacts: &[PocArtifact],
    ) -> Result<PocAttempt> {
        if result == PocResult::Confirmed {
            for role in [ArtifactRole::Attack, ArtifactRole::Control] {
                if !artifacts.iter().any(|artifact| artifact.role == role) {
                    return Err(Error::ConfirmationNeedsArtifact { role });
                }
            }
        }
        admit_artifacts(&self.artifact_root(), artifacts)?;
        self.write(|db| {
            let actor_id = actor_row(db, actor)?;
            let start = open_start(db, actor_id, id)?;
            testable(db, id, tested, start)?;
            let poc = PocId(db.insert(
                "INSERT INTO poc(candidate,contradictions,validation,start,result,evidence,actor,at) VALUES (?1,(SELECT contradictions FROM candidate WHERE id=?1),?2,?3,?4,?5,?6,?7)",
                params![id.0, tested.0, start.map(|id| id.0), result, evidence, actor_id, now()],
            )?);
            for (position, artifact) in artifacts.iter().enumerate() {
                db.execute(
                    "INSERT INTO poc_artifact(poc,position,reference,role) VALUES (?1,?2,?3,?4)",
                    params![poc.0, position, artifact.reference.as_str(), artifact.role],
                )?;
            }
            audit(db, actor, EventKind::PocRecorded, Detail::Record(EventRecord::Poc(poc)), &[Subject::Candidate(id)])?;
            pocs(db, concat!(poc_select!(), "p.id=?1"), params![poc.0])?
                .pop()
                .ok_or_else(|| corrupt("a recorded PoC is missing").into())
        })
    }

    pub fn validation_attempts(
        &self,
        id: CandidateId,
        page: Page,
    ) -> Result<Listing<ValidationAttempt>> {
        self.read(|db| {
            let attempts = rows(
                db,
                concat!(
                    validation_select!(),
                    "v.candidate=?1 AND v.id>?2 ORDER BY v.id LIMIT ?3"
                ),
                params![id.0, page.after, page.limit()],
                validation_row,
            )?;
            Ok(Listing::keyset(attempts, page))
        })
    }

    /// The candidate's latest verdict, test and finding, whether or not they stand.
    pub fn latest(&self, id: CandidateId) -> Result<Latest> {
        self.read(|db| {
            Ok(Latest {
                verdict: optional(
                    db,
                    concat!(
                        validation_select!(),
                        "v.candidate=?1 ORDER BY v.id DESC LIMIT 1"
                    ),
                    params![id.0],
                    validation_row,
                )?
                .map(|attempt| attempt.verdict),
                poc: optional(
                    db,
                    "SELECT result FROM poc WHERE candidate=?1 ORDER BY id DESC LIMIT 1",
                    params![id.0],
                    |row| Ok(enum_column(row, 0)?),
                )?,
                finding: report::latest_of(db, id)?,
            })
        })
    }

    pub fn poc_attempts(&self, id: CandidateId, page: Page) -> Result<Listing<PocAttempt>> {
        self.read(|db| {
            let attempts = pocs(
                db,
                concat!(
                    poc_select!(),
                    "p.candidate=?1 AND p.id>?2 ORDER BY p.id LIMIT ?3"
                ),
                params![id.0, page.after, page.limit()],
            )?;
            Ok(Listing::keyset(attempts, page))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::tests::{
            assess, child_agent, confidence, contradiction, contributes, end_job, fixture, member,
            new_flaw, new_premise, poc, scenario, shortlisted, status, tested, text, validated,
            verdict,
        },
        *,
    };
    use skyhook::job::JobState;

    /// A test a live job opened shows as proving and changes nothing owed; it settles even
    /// once its verdict is demoted, shows abandoned once its job ends unsettled, and settles
    /// nothing once a refuted member contradicts its scenario.
    #[test]
    fn an_open_test_shows_proving_until_its_job_ends() {
        let (_directory, store, actor) = fixture();
        let (agent, job) = child_agent(&store, JobState::Running);
        let prover = agent;

        let demoted = validated(&store, &actor);
        let verdict = tested(&store, demoted);
        assert!(matches!(
            store.start_poc(&actor, demoted, verdict),
            Err(Error::TestNeedsJob)
        ));
        let start = store.start_poc(&prover, demoted, verdict).unwrap();
        assert_eq!(
            store.open_tests().unwrap(),
            [OpenTest {
                start,
                candidate: demoted,
                validation: verdict,
                live: true,
            }]
        );
        assert_eq!(status(&store, demoted), Status::Validated);
        store
            .add_premise(
                &actor,
                scenario(demoted),
                &new_premise(InvariantKind::Bounds, "the tail exceeds the buffer"),
            )
            .unwrap();
        assert_eq!(status(&store, demoted), Status::Shortlisted);
        let settled = poc(&store, &prover, demoted, PocResult::Failed);
        assert_eq!(settled.start, Some(start));
        assert!(store.open_tests().unwrap().is_empty());

        let stale = validated(&store, &actor);
        let verdict = tested(&store, stale);
        store.start_poc(&prover, stale, verdict).unwrap();
        end_job(&store, job);
        assert!(!store.open_tests().unwrap()[0].live);
        let (flaw, _) = member(&store, stale);
        store
            .refute_flaw(&actor, flaw, &text("bounded upstream"))
            .unwrap();
        assert!(matches!(
            store.record_poc(
                &prover,
                stale,
                verdict,
                PocResult::Failed,
                &text("late"),
                &[]
            ),
            Err(Error::Contradicted)
        ));
    }

    /// Only a contradiction of the scenario reopens a confirmed test: material changes,
    /// a new relation and a merge leave it standing once the analysis is renewed; a
    /// condition gating the scenario found failing, or undermined, owes a new test.
    #[test]
    fn a_confirmed_test_stands_until_its_scenario_is_contradicted() {
        let (_directory, store, actor) = fixture();
        let id = validated(&store, &actor);
        let (flaw, _) = member(&store, id);
        let confirm = || poc(&store, &actor, id, PocResult::Confirmed);
        let renew = || {
            assess(&store, &actor, id);
            verdict(&store, &actor, id, Verdict::Validated);
        };
        confirm();
        let other = store
            .add_flaw(
                &actor,
                &new_flaw(store.flaw(flaw).unwrap().record.flaw.component, "dup"),
            )
            .unwrap();
        store
            .add_evidence(
                &actor,
                EvidenceSubject::Flaw(flaw),
                &contradiction("narrower"),
            )
            .unwrap();
        store
            .relate_flaws(
                &actor,
                other,
                flaw,
                RelationKind::Enables,
                &text("feeds it"),
            )
            .unwrap();
        let variant = shortlisted(&store, &actor);
        store
            .merge_candidates(&actor, variant, id, &[], &text("same scenario"))
            .unwrap();
        assert_eq!(status(&store, id), Status::Identified);
        contributes(&store, &actor, flaw, member(&store, id).1);
        renew();
        assert_eq!(confidence(&store, id), Some(ConfidenceTier::Demonstrated));

        let condition = store
            .add_premise(
                &actor,
                scenario(id),
                &new_premise(InvariantKind::State, "the stream resumes"),
            )
            .unwrap();
        let resolve = |resolution| {
            store
                .resolve_premise(&actor, condition, resolution, &text("traced"))
                .unwrap()
        };
        resolve(Resolution::Holds { deployment: None });
        renew();
        assert_eq!(confidence(&store, id), Some(ConfidenceTier::Demonstrated));
        resolve(Resolution::Fails { flaw: None });
        resolve(Resolution::Holds { deployment: None });
        renew();
        assert_eq!(
            confidence(&store, id),
            Some(ConfidenceTier::Confirmed),
            "the failure reopened the test"
        );
        confirm();
        store
            .undermine(
                &actor,
                other,
                Invariant::Premise(condition),
                &text("breaks it"),
            )
            .unwrap();
        resolve(Resolution::Holds { deployment: None });
        assert_eq!(
            confidence(&store, id),
            Some(ConfidenceTier::Confirmed),
            "the undermine reopened it"
        );
    }
}

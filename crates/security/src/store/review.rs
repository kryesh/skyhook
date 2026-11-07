//! Assessments and verdicts, each recorded by an event whose note is its rationale. The latest
//! recorded within a review of the candidate stands: an assessment current until the candidate
//! is edited, a verdict while nothing the review reads changed after it; one recorded elsewhere
//! is a proposal, which its review decides.

use schemars::JsonSchema;
use serde::Serialize;
use skyhook::db::params;

use super::*;
use crate::cvss::Cvss40;

/// What an assessment scores, and whether its review owes a verdict.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, JsonSchema)]
pub struct Assessment {
    pub severity: Severity,
    pub score: f64,
    pub shortlisted: bool,
}

const ASSESSMENT: Standing = standing_of!(
    "candidate_result",
    "candidate",
    "vector",
    "assessment_proposed",
    crate::store::bearing::owns!("review", "?2", "?1"),
    Standing
);
const VERDICT: Standing = standing_of!(
    "candidate_result",
    "candidate",
    "verdict",
    "verdict_proposed",
    crate::store::bearing::owns!("review", "?2", "?1"),
    Standing
);

impl Store {
    /// Score `id` at `vector`: its impact were every premise of its chain to hold.
    pub fn assess(
        &self,
        actor: &AgentId,
        id: CandidateId,
        vector: Cvss40,
        note: &Text,
    ) -> Result<Assessment> {
        let severity = vector.severity();
        self.write(|db| {
            let id = live(db, id)?;
            audit(db, actor, EventKind::Assessed, Some(note), |event| {
                let sql = "INSERT INTO candidate_assessment(event,candidate,vector,severity,owned) \
                           VALUES (?1,?2,?3,?4,?5)";
                let row = params![event, id, vector.to_string(), severity];
                let effect = ASSESSMENT.record(db, id, event, vector.to_string(), sql, row)?;
                let shortlisted = stored(
                    db,
                    "SELECT k.rank>=t.rank FROM run JOIN severity_kind t ON t.name=run.threshold \
                     JOIN severity_kind k ON k.name=?1",
                    params![severity],
                    |row| Ok(row.get::<bool>(0)?),
                )?;
                let assessment = Assessment {
                    severity,
                    score: vector.score(),
                    shortlisted,
                };
                Ok((assessment, Marks::of(id, effect)))
            })
        })
    }

    /// Judge `id` with the reviewer's reasons.
    pub fn verdict(
        &self,
        actor: &AgentId,
        id: CandidateId,
        verdict: Verdict,
        note: &Text,
    ) -> Result<()> {
        self.write(|db| {
            let id = live(db, id)?;
            audit(db, actor, EventKind::Verdict, Some(note), |event| {
                let sql = "INSERT INTO candidate_verdict(event,candidate,verdict,owned) VALUES \
                           (?1,?2,?3,?4)";
                let row = params![event, id, verdict];
                let effect = VERDICT.record(db, id, event, verdict, sql, row)?;
                Ok(((), Marks::of(id, effect)))
            })
        })
    }
}

//! Assessments and verdicts, recorded in place on the candidate. Each names the event that
//! recorded it, whose note is its rationale; it is current while nothing its phase reads
//! changed after it.

use schemars::JsonSchema;
use serde::Serialize;
use skyhook::db::params;

use super::*;
use crate::cvss::Cvss40;

/// What an assessment scores, and whether it is sent to review.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, JsonSchema)]
pub struct Assessment {
    pub severity: Severity,
    pub score: f64,
    pub shortlisted: bool,
}

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
                db.execute(
                    "UPDATE candidate SET vector=?2,severity=?3,assessed=?4 WHERE node=?1",
                    params![id, vector.to_string(), severity, event],
                )?;
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
                Ok((assessment, Marks::of(id, EventEffect::Standing)))
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
                db.execute(
                    "UPDATE candidate SET verdict=?2,reviewed=?3 WHERE node=?1",
                    params![id, verdict, event],
                )?;
                Ok(((), Marks::of(id, EventEffect::Standing)))
            })
        })
    }
}

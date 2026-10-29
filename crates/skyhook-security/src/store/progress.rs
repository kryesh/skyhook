//! Cheap, coherent progress counts.
use std::collections::BTreeMap;

use skyhook::{
    db::{Db, enum_column, optional_enum_column},
    named_enum::NamedEnum,
};

use super::*;

#[derive(Clone)]
pub struct Progress {
    pub principals: u64,
    pub sources: u64,
    pub paths: u64,
    pub components: u64,
    /// Components by their latest sweep's outcome.
    pub sweeps: BTreeMap<Outcome, u64>,
    /// Flaws neither merged nor refuted at their current revision.
    pub flaws: u64,
    /// Unmerged candidates by status.
    pub candidates: BTreeMap<Status, u64>,
    /// Unmerged candidates assessed at their current revision, by confidence.
    pub confidence: BTreeMap<ConfidenceTier, u64>,
}

impl Progress {
    pub fn candidates_in(&self, status: Status) -> u64 {
        self.candidates.get(&status).copied().unwrap_or_default()
    }

    pub fn candidates_at(&self, confidence: ConfidenceTier) -> u64 {
        self.confidence
            .get(&confidence)
            .copied()
            .unwrap_or_default()
    }
}

/// What the orchestrator steers the run by, read at once: the run, its plan and the
/// work owed.
#[derive(Clone)]
pub struct RunFacts {
    pub run: Run,
    pub synthesis: Synthesis,
    pub artifacts: std::path::PathBuf,
}

/// The run as the Research tab shows it: its facts and the tallies the page draws, read
/// at once.
#[derive(Clone)]
pub struct RunState {
    pub facts: RunFacts,
    pub progress: Progress,
    /// Root flaws a standing finding reports.
    pub findings: u64,
    /// Every finding recorded, superseded ones too.
    pub recorded_findings: u64,
    pub premises: u64,
    /// Unmerged flaws refuted at their current revision.
    pub refuted: u64,
    /// Whether any verdict was recorded, and any test: review and PoC work ran.
    pub reviewed: bool,
    pub tested: bool,
}

impl Store {
    pub fn run_facts(&self) -> Result<RunFacts> {
        self.read(|db| self.facts(db))
    }

    pub fn run_state(&self) -> Result<RunState> {
        self.read(|db| {
            Ok(RunState {
                facts: self.facts(db)?,
                progress: progress(db)?,
                findings: runs::findings(db)?,
                recorded_findings: count(db, "SELECT count(*) FROM finding", Vec::new())?,
                premises: count(db, "SELECT count(*) FROM premise", Vec::new())?,
                refuted: count(
                    db,
                    "SELECT count(*) FROM flaw_state WHERE merged_into IS NULL AND refuted",
                    Vec::new(),
                )?,
                reviewed: exists(db, "SELECT 1 FROM validation", Vec::new())?,
                tested: exists(db, "SELECT 1 FROM poc", Vec::new())?,
            })
        })
    }

    fn facts(&self, db: &Db) -> Result<RunFacts> {
        Ok(RunFacts {
            run: runs::run(db)?,
            synthesis: plan::synthesis(db)?,
            artifacts: self.artifact_root(),
        })
    }
}

fn progress(db: &Db) -> Result<Progress> {
    let mut candidates = BTreeMap::new();
    let mut confidence = BTreeMap::new();
    // One read of the standing view for both tallies.
    for (status, tier, count) in rows(
        db,
        "SELECT status,confidence,count(*) FROM candidate_standing WHERE status!='merged' GROUP \
         BY status,confidence",
        Vec::new(),
        |row| {
            Ok((
                enum_column::<Status>(row, 0)?,
                optional_enum_column::<ConfidenceTier>(row, 1)?,
                row.get::<i64>(2)?.max(0) as u64,
            ))
        },
    )? {
        *candidates.entry(status).or_default() += count;
        if let Some(tier) = tier {
            *confidence.entry(tier).or_default() += count;
        }
    }
    Ok(Progress {
        candidates,
        confidence,
        principals: count(db, "SELECT count(*) FROM principal", Vec::new())?,
        sources: count(db, "SELECT count(*) FROM source", Vec::new())?,
        paths: count(db, "SELECT count(*) FROM path", Vec::new())?,
        components: count(db, "SELECT count(*) FROM component", Vec::new())?,
        sweeps: counts(
            db,
            concat!(
                "SELECT outcome,count(*) FROM (SELECT ",
                components::latest_sweep!(),
                " AS outcome FROM component c) WHERE outcome IS NOT NULL GROUP BY outcome"
            ),
        )?,
        flaws: count(
            db,
            "SELECT count(*) FROM flaw_state WHERE current",
            Vec::new(),
        )?,
    })
}

pub(super) fn counts<T: NamedEnum + Ord>(db: &Db, sql: &'static str) -> Result<BTreeMap<T, u64>> {
    rows(db, sql, Vec::new(), |row| {
        Ok((enum_column(row, 0)?, row.get::<i64>(1)?.max(0) as u64))
    })
    .map(|pairs| pairs.into_iter().collect())
}

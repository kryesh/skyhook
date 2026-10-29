//! The Research tab's records: the first of each kind in the order a reader wants them,
//! with the counts their sections summarise. Detail comes from each record's own read.
use skyhook::db::{optional_enum_column, params};

use super::*;

#[derive(Clone, Debug)]
pub struct CandidateRow {
    pub candidate: CandidateSummary,
    pub members: u64,
    pub paths: u64,
}

#[derive(Clone, Debug)]
pub struct FindingRow {
    pub finding: Finding,
    /// The headline scenario's title.
    pub title: String,
    /// The headline's reported test.
    pub poc: Option<PocResult>,
}

#[derive(Clone, Debug)]
pub struct PathRow {
    pub id: PathId,
    pub description: String,
    pub sources: u64,
    pub components: u64,
    pub coverage: PathCoverage,
    pub review: ReviewState,
}

/// Where a path's latest review unit stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewState {
    NotPlanned,
    Open,
    Ended(Outcome),
}

/// A path's latest decision about one flaw, marked whether it still stands.
#[derive(Clone, Debug)]
pub struct PathExposure {
    pub flaw: FlawId,
    pub title: String,
    pub outcome: ExposureOutcome,
    pub current: bool,
}

#[derive(Clone, Debug)]
pub struct FlawRow {
    pub id: FlawId,
    pub title: String,
    pub component: String,
    pub operation: OperationKind,
    pub deficiency: Option<GuardDeficiency>,
    pub effects: Vec<Property>,
    /// Reaching paths still owing a decision on it.
    pub owed: u64,
    pub refuted: bool,
}

#[derive(Clone, Debug)]
pub struct ComponentRow {
    pub component: Component,
    pub files: u64,
    pub paths: u64,
    pub flaws: u64,
}

#[derive(Clone, Debug)]
pub struct SourceRow {
    pub source: Source,
    pub paths: u64,
}

#[derive(Clone, Debug)]
pub struct PremiseRow {
    pub id: PremiseId,
    pub on: PremiseOn,
    pub statement: String,
    /// Its resolution at its current revision; none while it is open.
    pub standing: Option<ResolutionKind>,
}

impl Store {
    /// Reviewed and shortlisted candidates first, the most severe first within a status.
    pub fn candidate_rows(&self, limit: usize) -> Result<Vec<CandidateRow>> {
        self.read(|db| {
            rows(
                db,
                concat!(
                    candidates::summary_select!(
                        "(SELECT count(*) FROM candidate_member WHERE candidate=c.id),\
                         (SELECT count(*) FROM candidate_path WHERE candidate=c.id)"
                    ),
                    "c.merged_into IS NULL ORDER BY CASE st.status WHEN 'validated' THEN 0 \
                     WHEN 'noted' THEN 1 WHEN 'reported' THEN 1 WHEN 'shortlisted' THEN 2 \
                     WHEN 'unproven' THEN 3 WHEN 'identified' THEN 4 WHEN 'below_threshold' THEN 5 \
                     ELSE 6 END, st.rank DESC, c.id LIMIT ?1"
                ),
                params![limit],
                |row| {
                    Ok(CandidateRow {
                        candidate: candidates::summary_row(row)?,
                        members: row.get::<i64>(7)?.max(0) as u64,
                        paths: row.get::<i64>(8)?.max(0) as u64,
                    })
                },
            )
        })
    }

    /// The newest findings first.
    pub fn finding_rows(&self, limit: usize) -> Result<Vec<FindingRow>> {
        self.read(|db| {
            (report::recent(db, limit)?.into_iter())
                .map(|finding| {
                    let (title, poc) = stored(
                        db,
                        "SELECT c.title,p.result FROM finding_candidate x JOIN candidate c ON \
                         c.id=x.candidate LEFT JOIN poc p ON p.id=x.poc WHERE x.finding=?1 AND \
                         x.candidate=?2",
                        params![finding.id.0, finding.headline.0],
                        |row| Ok((row.get(0)?, optional_enum_column(row, 1)?)),
                    )?;
                    Ok(FindingRow {
                        finding,
                        title,
                        poc,
                    })
                })
                .collect()
        })
    }

    /// Paths still owing review first, then those reaching nothing recorded, then checked.
    pub fn path_rows(&self, limit: usize) -> Result<Vec<PathRow>> {
        self.read(|db| {
            rows(
                db,
                concat!(
                    "SELECT p.id,p.description,(SELECT count(*) FROM path_source WHERE path=p.id),\
                     (SELECT count(*) FROM path_reach WHERE path=p.id),",
                    paths::path_coverage!(),
                    " AS coverage,r.id,(SELECT kind FROM outcome WHERE unit=r.id) FROM path p LEFT JOIN plan_unit r ON r.id=(SELECT max(id) FROM \
                     plan_unit WHERE kind='path_review' AND path=p.id) ORDER BY CASE coverage WHEN \
                     'unchecked' THEN 0 WHEN 'unreached' THEN 1 ELSE 2 END, p.id LIMIT ?1"
                ),
                params![limit],
                |row| {
                    Ok(PathRow {
                        id: PathId(row.get(0)?),
                        description: row.get(1)?,
                        sources: row.get::<i64>(2)?.max(0) as u64,
                        components: row.get::<i64>(3)?.max(0) as u64,
                        coverage: enum_column(row, 4)?,
                        review: match (row.get::<Option<i64>>(5)?, optional_enum_column(row, 6)?) {
                            (None, _) => ReviewState::NotPlanned,
                            (Some(_), None) => ReviewState::Open,
                            (Some(_), Some(outcome)) => ReviewState::Ended(outcome),
                        },
                    })
                },
            )
        })
    }

    /// The path's latest decision about each flaw.
    pub fn path_exposures(&self, path: PathId) -> Result<Vec<PathExposure>> {
        self.read(|db| {
            rows(
                db,
                "SELECT x.flaw,f.title,x.outcome,x.standing FROM exposure_decision x JOIN flaw f \
                 ON f.id=x.flaw WHERE x.path=?1 ORDER BY x.flaw",
                params![path.0],
                |row| {
                    Ok(PathExposure {
                        flaw: FlawId(row.get(0)?),
                        title: row.get(1)?,
                        outcome: enum_column(row, 2)?,
                        current: row.get::<i64>(3)? != 0,
                    })
                },
            )
        })
    }

    /// Open flaws with decisions still owed first, then open flaws, then refuted ones.
    pub fn flaw_rows(&self, limit: usize) -> Result<Vec<FlawRow>> {
        self.read(|db| {
            let flaws = rows(
                db,
                concat!(
                    "SELECT f.id,f.title,c.name,f.operation,f.guard,f.deficiency,f.refuted,\
                     (SELECT count(*) FROM path_reach r WHERE r.component=f.component AND ",
                    paths::undecided!("r.path"),
                    ") AS owed FROM flaw_state f JOIN component c ON c.id=f.component WHERE \
                     f.merged_into IS NULL ORDER BY 7, owed=0, f.id LIMIT ?1"
                ),
                params![limit],
                |row| {
                    Ok(FlawRow {
                        id: FlawId(row.get(0)?),
                        title: row.get(1)?,
                        component: row.get(2)?,
                        operation: enum_column(row, 3)?,
                        deficiency: flaws::deficiency_columns(row, 4)?,
                        effects: Vec::new(),
                        refuted: row.get::<i64>(6)? != 0,
                        owed: row.get::<i64>(7)?.max(0) as u64,
                    })
                },
            )?;
            flaws
                .into_iter()
                .map(|mut flaw| {
                    flaw.effects = rows(
                        db,
                        "SELECT DISTINCT property FROM flaw_effect WHERE flaw=?1 ORDER BY property",
                        params![flaw.id.0],
                        |row| Ok(enum_column(row, 0)?),
                    )?;
                    Ok(flaw)
                })
                .collect()
        })
    }

    /// Components whose sweep ended with a limitation first, then those never swept,
    /// then swept ones.
    pub fn component_rows(&self, limit: usize) -> Result<Vec<ComponentRow>> {
        self.read(|db| {
            rows(
                db,
                concat!(
                    components::component_select!(
                        "(SELECT count(*) FROM component_file WHERE component=c.id),\
                         (SELECT count(*) FROM path_reach WHERE component=c.id),\
                         (SELECT count(*) FROM flaw WHERE component=c.id AND merged_into IS NULL)"
                    ),
                    "1 ORDER BY CASE WHEN swept IS NULL THEN 1 WHEN swept='completed' THEN 2 \
                     ELSE 0 END, c.id LIMIT ?1"
                ),
                params![limit],
                |row| {
                    Ok(ComponentRow {
                        component: components::component_row(row)?,
                        files: row.get::<i64>(5)?.max(0) as u64,
                        paths: row.get::<i64>(6)?.max(0) as u64,
                        flaws: row.get::<i64>(7)?.max(0) as u64,
                    })
                },
            )
        })
    }

    pub fn source_rows(&self, limit: usize) -> Result<Vec<SourceRow>> {
        self.read(|db| {
            let ids = rows(
                db,
                "SELECT id,(SELECT count(*) FROM path_source WHERE source=s.id) FROM source s \
                 ORDER BY id LIMIT ?1",
                params![limit],
                |row| Ok((SourceId(row.get(0)?), row.get::<i64>(1)?.max(0) as u64)),
            )?;
            (ids.into_iter())
                .map(|(id, paths)| {
                    Ok(SourceRow {
                        source: scope::source(db, id)?,
                        paths,
                    })
                })
                .collect()
        })
    }

    /// Open conditions first.
    pub fn premise_rows(&self, limit: usize) -> Result<Vec<PremiseRow>> {
        self.read(|db| {
            rows(
                db,
                concat!(
                    "SELECT p.id,",
                    premises::owner_columns!(),
                    ",p.statement,",
                    premises::open_premise!(),
                    " AS open,(SELECT resolution FROM premise_decision WHERE premise=p.id AND \
                     current) FROM premise p ",
                    premises::owner_link!(),
                    " ORDER BY open DESC, p.id LIMIT ?1"
                ),
                params![limit],
                |row| {
                    Ok(PremiseRow {
                        id: PremiseId(row.get(0)?),
                        on: premises::on_columns(row, 1)?,
                        statement: row.get(6)?,
                        standing: optional_enum_column(row, 8)?,
                    })
                },
            )
        })
    }
}

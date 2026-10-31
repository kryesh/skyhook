//! Reports: one finding per root flaw, written in place, and what each current one states.

use serde::Serialize;
use skyhook::db::{Db, enum_column, params};

use super::*;
use crate::cvss::Cvss40;

impl Store {
    /// Record the report of root flaw `root`, or the flaw it was merged into: one finding per
    /// root, rewritten in place.
    pub fn report(
        &self,
        actor: &AgentId,
        root: FlawId,
        title: &Text,
        body: &Text,
    ) -> Result<FindingId> {
        self.write(|db| {
            let root = live(db, root)?;
            audit(db, actor, EventKind::Reported, None, |event| {
                let existing = optional(
                    db,
                    "SELECT node FROM finding WHERE root=?1",
                    params![root],
                    |row| Ok(FindingId(row.get(0)?)),
                )?;
                let finding = match existing {
                    Some(finding) => {
                        db.execute(
                            "UPDATE node SET title=?2,body=?3 WHERE id=?1",
                            params![finding, title, body],
                        )?;
                        db.execute(
                            "UPDATE finding SET reported=?2 WHERE node=?1",
                            params![finding, event],
                        )?;
                        finding
                    }
                    None => {
                        let finding = FindingId(db.insert(
                            "INSERT INTO node(kind,title,body) VALUES (?1,?2,?3)",
                            params![NodeKind::Finding, title, body],
                        )?);
                        db.execute(
                            "INSERT INTO finding(node,root,reported) VALUES (?1,?2,?3)",
                            params![finding, root, event],
                        )?;
                        finding
                    }
                };
                let mut marks = Marks::of(finding, EventEffect::Changed);
                for candidate in rows(
                    db,
                    "SELECT r.candidate FROM candidate_root r JOIN node n ON n.id=r.candidate WHERE \
                     r.root=?1 AND n.merged_into IS NULL",
                    params![root],
                    |row| Ok(NodeId(row.get(0)?)),
                )? {
                    marks.mark(candidate, EventEffect::Concerns);
                }
                Ok((finding, marks))
            })
        })
    }
}

/// A test a current report states: its scenario's settling test.
#[derive(Clone, Debug, Serialize)]
pub struct ReportedTest {
    pub id: TestId,
    pub result: TestResult,
    pub note: Option<String>,
    /// The instructions of the environment it ran in.
    pub environment: Option<String>,
    pub artifacts: Vec<RecordedArtifact>,
}

/// An artifact as its test recorded it, with the digest its contents had then.
#[derive(Clone, Debug, Serialize)]
pub struct RecordedArtifact {
    pub reference: ArtifactReference,
    pub role: ArtifactRole,
    #[serde(serialize_with = "hex")]
    pub digest: [u8; 32],
}

fn hex<S: serde::Serializer>(
    digest: &[u8; 32],
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.collect_str(
        &digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
    )
}

/// A scenario a current report states.
#[derive(Clone, Debug, Serialize)]
pub struct ReportedScenario {
    pub candidate: CandidateId,
    pub title: String,
    pub path: PathId,
    pub path_title: String,
    /// Its flaws from the root.
    pub chain: Vec<FlawId>,
    pub vector: Cvss40,
    pub severity: Severity,
    pub state: Status,
    pub confidence: ConfidenceTier,
    pub test: Option<ReportedTest>,
}

/// A root's finding that reports its scenarios as they stand.
#[derive(Clone, Debug, Serialize)]
pub struct CurrentReport {
    pub finding: FindingId,
    pub root: FlawId,
    pub cwe: Option<u32>,
    pub title: String,
    pub body: String,
    /// The most severe first.
    pub scenarios: Vec<ReportedScenario>,
}

fn scenarios(db: &Db, root: FlawId) -> Result<Vec<ReportedScenario>> {
    let mut scenarios = rows(
        db,
        "SELECT s.candidate,n.title,c.path,p.title,c.vector,s.severity,s.state,s.confidence FROM \
         finding_scenario s JOIN candidate c ON c.node=s.candidate JOIN node n ON \
         n.id=s.candidate JOIN node p ON p.id=c.path WHERE s.root=?1 ORDER BY s.rank DESC,\
         s.candidate",
        params![root],
        |row| {
            Ok(ReportedScenario {
                candidate: CandidateId(row.get(0)?),
                title: row.get(1)?,
                path: PathId(row.get(2)?),
                path_title: row.get(3)?,
                vector: (row.get::<String>(4)?)
                    .parse()
                    .map_err(|_| corrupt("invalid stored vector"))?,
                severity: enum_column(row, 5)?,
                state: enum_column(row, 6)?,
                confidence: enum_column(row, 7)?,
                chain: Vec::new(),
                test: None,
            })
        },
    )?;
    for scenario in &mut scenarios {
        scenario.chain = rows(
            db,
            "SELECT flaw FROM candidate_flaw WHERE candidate=?1 ORDER BY position",
            params![scenario.candidate],
            |row| Ok(FlawId(row.get(0)?)),
        )?;
        scenario.test = optional(
            db,
            concat!(
                "SELECT t.id,t.result,e.note,v.instructions FROM test t JOIN event e ON \
                 e.id=t.event LEFT JOIN environment v ON v.id=t.environment WHERE t.id=",
                crate::store::bearing::settling_test!("?1")
            ),
            params![scenario.candidate],
            |row| {
                Ok(ReportedTest {
                    id: TestId(row.get(0)?),
                    result: enum_column(row, 1)?,
                    note: row.get(2)?,
                    environment: row.get(3)?,
                    artifacts: Vec::new(),
                })
            },
        )?;
        if let Some(test) = &mut scenario.test {
            test.artifacts = rows(
                db,
                "SELECT path,role,digest FROM test_artifact WHERE test=?1 ORDER BY path",
                params![test.id],
                |row| {
                    Ok(RecordedArtifact {
                        reference: (row.get::<String>(0)?).parse()?,
                        role: enum_column(row, 1)?,
                        digest: (row.get::<Vec<u8>>(2)?)
                            .try_into()
                            .map_err(|_| corrupt("invalid stored digest"))?,
                    })
                },
            )?;
        }
    }
    Ok(scenarios)
}

impl Store {
    /// Every current finding, oldest first, with what it states of its scenarios.
    pub fn current_reports(&self) -> Result<Vec<CurrentReport>> {
        self.read(|db| {
            let mut reports = rows(
                db,
                concat!(
                    "SELECT f.node,f.root,fl.cwe,n.title,n.body FROM finding f JOIN node n ON \
                     n.id=f.node JOIN flaw fl ON fl.node=f.root WHERE ",
                    crate::store::queues::current_finding!("f.node"),
                    " ORDER BY f.node"
                ),
                Vec::new(),
                |row| {
                    Ok(CurrentReport {
                        finding: FindingId(row.get(0)?),
                        root: FlawId(row.get(1)?),
                        cwe: row.get(2)?,
                        title: row.get(3)?,
                        body: row.get(4)?,
                        scenarios: Vec::new(),
                    })
                },
            )?;
            for report in &mut reports {
                report.scenarios = scenarios(db, report.root)?;
            }
            Ok(reports)
        })
    }
}

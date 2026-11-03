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
                bearing::record_scenarios(db, event, root)?;
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

/// A path a reported scenario performs.
#[derive(Clone, Debug, Serialize)]
pub struct ReportedAction {
    pub path: PathId,
    pub title: String,
}

/// A scenario a current report states.
#[derive(Clone, Debug, Serialize)]
pub struct ReportedScenario {
    pub candidate: CandidateId,
    pub title: String,
    /// The paths it performs, in order.
    pub actions: Vec<ReportedAction>,
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
        "SELECT s.candidate,n.title,c.vector,s.severity,s.state,s.confidence FROM \
         finding_scenario s JOIN candidate_result c ON c.candidate=s.candidate JOIN node n ON \
         n.id=s.candidate WHERE s.root=?1 ORDER BY s.rank DESC,s.candidate",
        params![root],
        |row| {
            Ok(ReportedScenario {
                candidate: CandidateId(row.get(0)?),
                title: row.get(1)?,
                vector: (row.get::<String>(2)?)
                    .parse()
                    .map_err(|_| corrupt("invalid stored vector"))?,
                severity: enum_column(row, 3)?,
                state: enum_column(row, 4)?,
                confidence: enum_column(row, 5)?,
                actions: Vec::new(),
                chain: Vec::new(),
                test: None,
            })
        },
    )?;
    for scenario in &mut scenarios {
        scenario.actions = rows(
            db,
            "SELECT a.path,n.title FROM candidate_action a JOIN node n ON n.id=a.path WHERE \
             a.candidate=?1 ORDER BY a.position",
            params![scenario.candidate],
            |row| {
                Ok(ReportedAction {
                    path: PathId(row.get(0)?),
                    title: row.get(1)?,
                })
            },
        )?;
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

/// Why a confirmed flaw is hardening rather than a finding.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HardeningReason {
    /// No validated candidate holds it: why, as the judgement confirming it noted.
    NoChain { note: String },
    /// Its most severe validated candidate is assessed below the run's threshold.
    BelowThreshold {
        candidate: CandidateId,
        severity: Severity,
    },
}

/// A confirmed flaw no current finding reports: hardening, not a finding.
#[derive(Clone, Debug, Serialize)]
pub struct Hardening {
    pub flaw: FlawId,
    pub title: String,
    pub location: Location,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwe: Option<u32>,
    pub reason: HardeningReason,
}

/// What the run's reports render, as one snapshot holds it.
pub struct Reporting {
    /// Every current finding, oldest first, with what it states of its scenarios.
    pub reports: Vec<CurrentReport>,
    /// Every live confirmed flaw no scenario of a finding holds, oldest first.
    pub hardening: Vec<Hardening>,
}

impl Store {
    pub fn reporting(&self) -> Result<Reporting> {
        self.read(|db| {
            Ok(Reporting {
                reports: current_reports(db)?,
                hardening: hardening(db)?,
            })
        })
    }
}

fn hardening(db: &Db) -> Result<Vec<Hardening>> {
    rows(
        db,
        concat!(
            "SELECT f.node,n.title,f.cwe,e.note,b.candidate,b.severity,",
            location_columns!(),
            " FROM flaw_standing f JOIN node n ON n.id=f.node JOIN event e ON e.id=f.judged \
             LEFT JOIN (SELECT uf.flaw,u.candidate,u.severity,row_number() OVER (PARTITION BY \
             uf.flaw ORDER BY u.rank DESC,u.candidate) AS nth FROM candidate_flaw uf JOIN \
             candidate_core u ON u.candidate=uf.candidate WHERE u.upheld AND NOT u.shortlisted \
             AND u.merged_into IS NULL) b ON b.flaw=f.node AND b.nth=1",
            location_joins!("f.location"),
            " WHERE f.status=?1 AND n.merged_into IS NULL AND NOT EXISTS (SELECT 1 FROM \
             candidate_flaw cf JOIN finding_scenario s ON s.candidate=cf.candidate WHERE \
             cf.flaw=f.node) ORDER BY f.node"
        ),
        params![FlawStatus::Confirmed],
        |row| {
            let reason = match row.get::<Option<i64>>(4)? {
                Some(candidate) => HardeningReason::BelowThreshold {
                    candidate: CandidateId(candidate),
                    severity: enum_column(row, 5)?,
                },
                None => HardeningReason::NoChain { note: row.get(3)? },
            };
            Ok(Hardening {
                flaw: FlawId(row.get(0)?),
                title: row.get(1)?,
                cwe: row.get(2)?,
                reason,
                location: located(row, 6)?,
            })
        },
    )
}

fn current_reports(db: &Db) -> Result<Vec<CurrentReport>> {
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
}

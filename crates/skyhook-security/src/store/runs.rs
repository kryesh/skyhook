use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Serialize;
use skyhook::{
    db::{Db, DbResult, Dictionaries, Seeding, enum_column, params},
    identity::SessionId,
    session::{Extension, ExtensionDb, SessionError},
};

use super::*;
use crate::cvss::BaseMetric;

/// The research tables every security session keeps beside its journal.
pub const EXTENSION: Extension = Extension {
    sql: include_str!("schema.sql"),
    format: 1,
    seed,
};

fn seed(db: &Db, seeding: Seeding) -> DbResult<()> {
    let dictionaries = Dictionaries(db, seeding);
    dictionaries.names("poc_policy_kind", &PocPolicy::ALL)?;
    dictionaries.ranked("severity_kind", &Severity::ALL)?;
    dictionaries.names("confidence_kind", &Confidence::ALL)?;
    dictionaries.names("evidence_role", &EvidenceRole::ALL)?;
    dictionaries.names("evidence_stance", &Stance::ALL)?;
    dictionaries.names("relation_kind", &RelationKind::ALL)?;
    dictionaries.names("verdict_kind", &VerdictKind::ALL)?;
    dictionaries.names("rejection_reason", &RejectionReason::ALL)?;
    dictionaries.names("poc_result_kind", &PocResult::ALL)?;
    dictionaries.names("artifact_role", &ArtifactRole::ALL)?;
    dictionaries.names("instruction_part", &InstructionPart::ALL)?;
    dictionaries.names("event_kind", &EventKind::ALL)?;
    dictionaries.names("credential_reference_kind", &CredentialReferenceKind::ALL)?;
    dictionaries.names("source_kind", &SourceKind::ALL)?;
    dictionaries.flagged("trust_kind", ["untrusted"], &Trust::ALL, |trust| {
        [trust.untrusted()]
    })?;
    dictionaries.flagged(
        "invariant_kind",
        ["load_bearing"],
        &InvariantKind::ALL,
        |kind| [kind.load_bearing()],
    )?;
    dictionaries.names("misreport_kind", &Misreport::ALL)?;
    dictionaries.names("retry_class", &RetryClass::ALL)?;
    dictionaries.names("unit_kind", &UnitKind::ALL)?;
    dictionaries.names("outcome_kind", &Outcome::ALL)?;
    dictionaries.names("operation_kind", &OperationKind::ALL)?;
    dictionaries.names("guard_kind", &GuardKind::ALL)?;
    dictionaries.names("deficiency_kind", &DeficiencyKind::ALL)?;
    dictionaries.names("property_kind", &Property::ALL)?;
    dictionaries.names("resolution_kind", &ResolutionKind::ALL)?;
    dictionaries.names("deployment_kind", &DeploymentKind::ALL)?;
    dictionaries.names("deployment_stance", &DeploymentStance::ALL)?;
    dictionaries.names("tree_state", &Tree::ALL)?;
    dictionaries.names("exposure_outcome", &ExposureOutcome::ALL)?;
    dictionaries.names("route_basis", &RouteBasis::ALL)?;
    dictionaries.names("gate_kind", &GateKind::ALL)?;
    dictionaries.names("cvss_metric", &BaseMetric::ALL)
}

/// The root flaws whose latest finding is not withdrawn.
pub(super) fn findings(db: &Db) -> Result<u64> {
    count(
        db,
        concat!(
            "SELECT count(*) FROM finding f WHERE ",
            report::latest_finding!("f"),
            " AND ",
            queues::group_reportable!("f.root")
        ),
        Vec::new(),
    )
}

pub(super) fn revision_columns(row: &libsql::Row, offset: i32) -> Result<Revision> {
    Ok(Revision {
        commit: row
            .get::<Option<String>>(offset)?
            .map(|text| text.parse())
            .transpose()?,
        tree: enum_column(row, offset + 1)?,
    })
}

/// The run: its session, when it began, the revision it started at, its options, and the PoC
/// policy of the launch working it. A report states it without what a later launch changes.
#[derive(Clone, Debug, Serialize)]
pub struct Run {
    pub id: SessionId,
    pub created: DateTime<Utc>,
    pub revision: Revision,
    #[serde(flatten)]
    pub options: RunOptions,
    #[serde(skip)]
    pub poc: PocPolicy,
}

pub(super) fn run(db: &Db) -> Result<Run> {
    optional(
        db,
        "SELECT s.public_id,r.created,r.revision,r.tree,r.threshold,r.workers,r.poc FROM run r \
         JOIN session s",
        Vec::new(),
        |row| {
            Ok(Run {
                id: session_column(row, 0)?,
                created: time(row.get(1)?)?,
                revision: revision_columns(row, 2)?,
                options: RunOptions {
                    threshold: enum_column(row, 4)?,
                    workers: u16::try_from(row.get::<i64>(5)?)
                        .ok()
                        .and_then(std::num::NonZeroU16::new)
                        .ok_or_else(|| corrupt("invalid stored worker count"))?,
                },
                poc: enum_column(row, 6)?,
            })
        },
    )?
    .ok_or_else(|| Record::Run.missing())
}

impl Store {
    /// The research tables of an open session, which its harness opened with
    /// [`EXTENSION`].
    pub fn of(db: &ExtensionDb) -> Self {
        Self(db.clone())
    }

    /// A security session's research, read-only, whether or not it is open elsewhere.
    pub async fn inspect(root: &Path, id: SessionId) -> std::result::Result<Self, SessionError> {
        Ok(Self(ExtensionDb::inspect(root, id, EXTENSION).await?))
    }

    /// The session, which is the run.
    pub fn id(&self) -> SessionId {
        self.0.session()
    }

    /// The count of research writes committed so far through the owning session: what a
    /// read derives stays current while it holds. An inspected run has none.
    pub fn generation(&self) -> Option<u64> {
        self.0.generation()
    }

    /// The session's directory; the run's files live in its subdirectories.
    pub fn directory(&self) -> &Path {
        self.0.directory()
    }

    /// Where this run's PoC artifacts live; `record_poc` references resolve below it.
    pub fn artifact_root(&self) -> PathBuf {
        self.directory().join("artifacts")
    }

    /// Where this run's current reports are written, a folder per root flaw.
    pub fn report_root(&self) -> PathBuf {
        self.directory().join("reports")
    }

    /// Whether the run is recorded: setting it up is the session's first write, which an
    /// interruption can leave undone.
    pub fn initialized(&self) -> Result<bool> {
        self.read(|db| exists(db, "SELECT 1 FROM run", Vec::new()))
    }

    /// Record the run as its session opens, at `revision` under the launch's PoC policy.
    pub fn start_run(
        &self,
        options: RunOptions,
        revision: &Revision,
        poc: PocPolicy,
    ) -> Result<()> {
        self.write(|db| {
            db.execute(
                "INSERT INTO run(id,created,threshold,workers,revision,tree,poc) VALUES (1,?1,?2,?3,?4,?5,?6)",
                params![
                    now(),
                    options.threshold,
                    options.workers.get(),
                    revision.commit.map(|commit| commit.to_string()),
                    revision.tree,
                    poc
                ],
            )?;
            Ok(())
        })
    }

    /// A later launch works the run under its own PoC policy.
    pub fn set_poc_policy(&self, poc: PocPolicy) -> Result<()> {
        self.write(|db| {
            db.execute("UPDATE run SET poc=?1", params![poc])?;
            Ok(())
        })
    }

    /// Replace the run's worker count: the pool size later fan-out reads.
    pub fn set_workers(&self, workers: std::num::NonZeroU16) -> Result<()> {
        self.write(|db| {
            db.execute("UPDATE run SET workers=?1", params![workers.get()])?;
            Ok(())
        })
    }

    pub fn run(&self) -> Result<Run> {
        self.read(run)
    }

    /// How many root flaws a standing finding reports.
    pub fn finding_count(&self) -> Result<u64> {
        self.read(findings)
    }
}

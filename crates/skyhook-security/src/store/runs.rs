use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Serialize;
use skyhook::{
    db::{Db, DbResult, Dictionaries, Seeding, bytes, enum_column, params},
    identity::SessionId,
    session::{Extension, ExtensionDb, SessionError},
};

use super::*;

/// The research tables every security session keeps beside its journal.
pub const EXTENSION: Extension = Extension {
    sql: concat!(include_str!("schema.sql"), bearing::derived_views!()),
    format: 13,
    seed,
};

fn seed(db: &Db, seeding: Seeding) -> DbResult<()> {
    let dictionaries = Dictionaries(db, seeding);
    dictionaries.names("node_kind", &NodeKind::ALL)?;
    dictionaries.columns("task_phase", ["subject", "stage"], &Phase::ALL, |phase| {
        [
            libsql::Value::Text(phase.subject().as_str().to_owned()),
            libsql::Value::Integer(phase.stage().into()),
        ]
    })?;
    dictionaries.names("location_kind", &LocationKind::ALL)?;
    dictionaries.names("trust_kind", &Trust::ALL)?;
    dictionaries.names("premise_scope", &PremiseScope::ALL)?;
    dictionaries.names("premise_status", &PremiseStatus::ALL)?;
    dictionaries.names("flaw_status", &FlawStatus::ALL)?;
    dictionaries.names("path_status", &PathStatus::ALL)?;
    dictionaries.names("inventory_status", &InventoryStatus::ALL)?;
    dictionaries.ranked("severity_kind", &Severity::ALL)?;
    dictionaries.names("verdict_kind", &Verdict::ALL)?;
    dictionaries.names("stance_kind", &Stance::ALL)?;
    dictionaries.names("test_result", &TestResult::ALL)?;
    dictionaries.names("artifact_role", &ArtifactRole::ALL)?;
    dictionaries.names("event_kind", &EventKind::ALL)?;
    dictionaries.ranked("event_effect", &EventEffect::ALL)?;
    dictionaries.names("live_policy_kind", &LivePolicy::ALL)
}

/// The run: its session, when it began, its options, and the live policy it started under,
/// which it keeps.
#[derive(Clone, Debug, Serialize)]
pub struct Run {
    pub id: SessionId,
    pub created: DateTime<Utc>,
    #[serde(flatten)]
    pub options: RunOptions,
    #[serde(skip)]
    pub live: LivePolicy,
}

pub(super) fn run(db: &Db) -> Result<Run> {
    optional(
        db,
        "SELECT s.public_id,r.created,r.threshold,r.workers,r.live FROM run r JOIN session s",
        Vec::new(),
        |row| {
            Ok(Run {
                id: SessionId::from_bytes(bytes(row.get(0)?)?),
                created: time(row.get(1)?)?,
                options: RunOptions {
                    threshold: enum_column(row, 2)?,
                    workers: u16::try_from(row.get::<i64>(3)?)
                        .ok()
                        .and_then(std::num::NonZeroU16::new)
                        .ok_or_else(|| corrupt("invalid stored worker count"))?,
                },
                live: enum_column(row, 4)?,
            })
        },
    )?
    .ok_or(Error::NotFound {
        record: Record::Run,
        id: None,
    })
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

    /// Where this run's test artifacts live; test records reference files below it.
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

    /// Record the run as its session opens, under the live policy it keeps.
    pub fn start_run(&self, options: RunOptions, live: LivePolicy) -> Result<()> {
        self.write(|db| {
            db.execute(
                "INSERT INTO run(id,created,threshold,workers,live) VALUES (1,?1,?2,?3,?4)",
                params![now(), options.threshold, options.workers.get(), live],
            )?;
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
}

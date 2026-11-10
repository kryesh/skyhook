use std::{
    num::NonZeroU16,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde::Serialize;
use skyhook::identity::SessionId;
use skyhook::{
    db::{Db, DbResult, Dictionaries, Seeding, bytes, enum_column, params},
    session::{ExtensionDb, ProductImage, Schema, SessionError},
};
use skyhook_base::enums::NamedEnum;

use super::*;
use crate::cvss::Severity;

/// The research tables every security session keeps beside its journal.
pub(crate) const SCHEMA: Schema = Schema::extended(
    ProductImage::layered(
        concat!(
            include_str!("schema.sql"),
            crate::store::ledger::views::derived_views!()
        ),
        30,
    ),
    seed,
);

fn seed(db: &Db, seeding: Seeding) -> DbResult<()> {
    let dictionaries = Dictionaries(db, seeding);
    dictionaries.names("node_kind", &NodeKind::ALL)?;
    dictionaries.columns("task_phase", ["subject"], &Phase::ALL, |phase| {
        [libsql::Value::Text(phase.subject().as_str().to_owned())]
    })?;
    dictionaries.names("location_kind", &LocationKind::ALL)?;
    dictionaries.names("trust_kind", &Trust::ALL)?;
    dictionaries.names("state_scope", &StateScope::ALL)?;
    dictionaries.names("state_access", &Access::ALL)?;
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
    dictionaries.names("bearing_role", &BearingRole::ALL)?;
    dictionaries.names("live_policy_kind", &LivePolicy::ALL)
}

/// The run: its session, when it began, and its options.
#[derive(Clone, Debug)]
pub struct Run {
    pub id: SessionId,
    pub created: DateTime<Utc>,
    pub(crate) options: RunOptions,
}

/// The run, once its session recorded it.
fn started(db: &Db) -> Result<Option<Run>> {
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
                    live: enum_column(row, 4)?,
                },
            })
        },
    )
}

pub(super) fn run(db: &Db) -> Result<Run> {
    started(db)?.ok_or(Error::NotFound {
        record: Record::Run,
        id: None,
    })
}

impl Store {
    /// The research tables of an open session, which its harness opened with
    /// [`SCHEMA`].
    pub(crate) fn of(db: &ExtensionDb) -> Self {
        Self(db.clone())
    }

    /// The research of a session, when its harness opened it with [`SCHEMA`].
    pub(crate) fn of_session(db: Option<&ExtensionDb>) -> Result<Self> {
        db.map(Self::of).ok_or(Error::NoResearch)
    }

    /// A security session's research, read-only, whether or not it is open elsewhere.
    pub async fn inspect(root: &Path, id: SessionId) -> std::result::Result<Self, SessionError> {
        Ok(Self(ExtensionDb::inspect(root, id, SCHEMA).await?))
    }

    /// The session, which is the run.
    pub(crate) fn id(&self) -> SessionId {
        self.0.session()
    }

    /// The count of research writes committed so far through the owning session: what a
    /// read derives stays current while it holds. An inspected run has none.
    pub(crate) fn generation(&self) -> Option<u64> {
        self.0.generation()
    }

    /// The session's directory; the run's files live in its subdirectories.
    pub(crate) fn directory(&self) -> &Path {
        self.0.directory()
    }

    /// Where this run's test artifacts live; test records reference files below it.
    pub(crate) fn artifact_root(&self) -> PathBuf {
        self.directory().join("artifacts")
    }

    /// Where this run's current reports are written, a folder per root flaw.
    pub(crate) fn report_root(&self) -> PathBuf {
        self.directory().join("reports")
    }

    /// The run, once recorded: setting it up is the session's first write, which an
    /// interruption can leave undone.
    pub(crate) fn started(&self) -> Result<Option<Run>> {
        self.read(started)
    }

    /// Record the run as its session opens.
    pub(crate) fn start_run(&self, options: RunOptions) -> Result<()> {
        self.write(|db| {
            db.execute(
                "INSERT INTO run(id,created,threshold,workers,live) VALUES (1,?1,?2,?3,?4)",
                params![
                    now(),
                    options.threshold,
                    options.workers.get(),
                    options.live
                ],
            )?;
            Ok(())
        })
    }

    /// Replace the run's worker count: the pool size later fan-out reads.
    pub(crate) fn set_workers(&self, workers: std::num::NonZeroU16) -> Result<()> {
        self.write(|db| {
            db.execute("UPDATE run SET workers=?1", params![workers.get()])?;
            Ok(())
        })
    }
}

/// Whether a launch works against a running target: probing it in an environment serving its
/// interfaces and demonstrating validated candidates. Static analysis of binaries is reading,
/// allowed under any.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub enum LivePolicy {
    Ask,
    Yes,
    No,
}

/// A run's settings, persisted with it; a later launch may change its workers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RunOptions {
    /// The lowest conditional severity a review goes on to judge.
    pub(crate) threshold: Severity,
    /// How many agents the orchestrator's scheduler runs at once.
    pub(crate) workers: NonZeroU16,
    /// The live policy the run keeps.
    pub(crate) live: LivePolicy,
}
impl Default for RunOptions {
    /// A run doing no live work.
    fn default() -> Self {
        Self {
            threshold: Severity::High,
            workers: const { NonZeroU16::new(6).unwrap() },
            live: LivePolicy::No,
        }
    }
}

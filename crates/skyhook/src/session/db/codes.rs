//! Normalized dictionaries: fixed built-in codes and extensible host capability names.

use std::collections::BTreeSet;

use super::*;
use crate::{
    agent::{FailureKind, FaultKind},
    tool::policy::Capability,
};

impl Sql for &Capability {
    fn sql(self) -> Value {
        self.as_str().sql()
    }
}

/// Names are journal data, independent of the host declarations that admitted them.
pub(super) fn record_capability(db: &Db, capability: &Capability) -> DbResult<()> {
    db.execute(
        "INSERT INTO capability (name) VALUES (?1) ON CONFLICT (name) DO NOTHING",
        params![capability],
    )
    .map(drop)
}

/// Seed each dictionary with its enum's spellings and flags, or check them.
pub(super) fn seed(dictionaries: &Dictionaries<'_>) -> DbResult<()> {
    let Dictionaries(db, seeding) = *dictionaries;
    use crate::{
        agent::TodoStatus,
        job::{AnswerKind, CaptureKind, JobRole, JobState},
        media::ImageFormat,
        provider::{
            profile::StateMode,
            protocol::{Binding, HistoryLifetime, ItemKind, ReplayFormat},
        },
        session::{EntryKind, ModelPurpose, TitleSource, Truncation, event::ResponseOutcome},
        target::{SshAuth, TargetSource},
        tool::diagnostic::{Effects, IoKind, Operation, PathRole, RequestEffects},
    };
    capabilities(db, seeding)?;
    dictionaries.flagged("entry_kind", ["activity"], &EntryKind::ALL, |kind| {
        [kind.is_activity()]
    })?;
    dictionaries.names("title_source", &TitleSource::ALL)?;
    dictionaries.names("target_source", &TargetSource::ALL)?;
    dictionaries.names("ssh_auth", &SshAuth::ALL)?;
    dictionaries.names("state_mode", &StateMode::ALL)?;
    dictionaries.names("model_purpose", &ModelPurpose::ALL)?;
    dictionaries.names("message_role", &message::MessageRole::ALL)?;
    dictionaries.names("image_format", &ImageFormat::ALL)?;
    dictionaries.names("user_part_kind", &message::UserPartKind::ALL)?;
    dictionaries.names("job_event_kind", &message::JobEventKind::ALL)?;
    dictionaries.names("item_kind", &ItemKind::ALL)?;
    dictionaries.names("replay_binding", &Binding::ALL)?;
    dictionaries.names("replay_format", &ReplayFormat::ALL)?;
    dictionaries.names("todo_status", &TodoStatus::ALL)?;
    dictionaries.names("history_lifetime", &HistoryLifetime::ALL)?;
    dictionaries.names("response_outcome", &ResponseOutcome::ALL)?;
    dictionaries.names("cut_reason", &Truncation::ALL)?;
    dictionaries.names("job_role", &JobRole::ALL)?;
    dictionaries.names("capture_kind", &CaptureKind::ALL)?;
    dictionaries.names("capture_detection", &crate::job::output::Detection::ALL)?;
    dictionaries.names(
        "field_presentation",
        &crate::tool::output::FieldPresentation::ALL,
    )?;
    dictionaries.names("diagnostic_operation", &Operation::ALL)?;
    dictionaries.names("diagnostic_subject", &diagnostic::SubjectKind::ALL)?;
    dictionaries.names("diagnostic_site", &diagnostic::SiteKind::ALL)?;
    dictionaries.names("diagnostic_effects", &Effects::ALL)?;
    dictionaries.names("diagnostic_request_effects", &RequestEffects::ALL)?;
    dictionaries.names("diagnostic_cause", &diagnostic::CauseKind::ALL)?;
    dictionaries.names("diagnostic_io_kind", &IoKind::ALL)?;
    dictionaries.names("diagnostic_path_role", &PathRole::ALL)?;
    dictionaries.flagged("job_state", ["terminal"], &JobState::ALL, |state| {
        [state.is_terminal()]
    })?;
    dictionaries.names("job_answer", &AnswerKind::ALL)?;
    dictionaries.names("request_phase", &request::PhaseKind::ALL)?;
    dictionaries.names("agent_last", &state::LastKind::ALL)?;
    dictionaries.names("agent_outcome", &state::OutcomeKind::ALL)?;
    dictionaries.names("failure_kind", &FailureKind::ALL)?;
    dictionaries.names(
        "provider_error_kind",
        &crate::provider::ProviderErrorKind::ALL,
    )?;
    dictionaries.names("compaction_fault", &FaultKind::ALL)?;
    dictionaries.names("checkpoint_error", &crate::session::CheckpointError::ALL)?;
    Ok(())
}

/// Built-ins are always present; host names only need to parse.
fn capabilities(db: &Db, seeding: Seeding) -> DbResult<()> {
    if let Seeding::Fill = seeding {
        for capability in &Capability::ALL {
            record_capability(db, capability)?;
        }
    }
    let recorded: BTreeSet<Capability> = db
        .query("SELECT name FROM capability", Vec::new(), |row| {
            row.get::<String>(0)?
                .parse()
                .map_err(|_| DbError::Dictionary("capability"))
        })?
        .into_iter()
        .collect();
    if Capability::ALL
        .iter()
        .any(|builtin| !recorded.contains(builtin))
    {
        return Err(DbError::Dictionary("capability"));
    }
    Ok(())
}

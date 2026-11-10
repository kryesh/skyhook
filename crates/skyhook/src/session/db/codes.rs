//! Normalized dictionaries: fixed built-in codes and extensible host capability names.

use std::collections::BTreeSet;

use super::*;
use crate::session::{FailureKind, FaultKind};
use crate::tool::policy::Capability;

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
    use crate::job::JobRole;
    use crate::target::{ConnectionKind, SshAuthKind, TargetSource};
    use crate::{
        job::{AnswerKind, JobState},
        provider::{
            model::{Effort, StateMode},
            protocol::{Binding, HistoryLifetime, ItemKind, ReplayFormat},
        },
        session::TodoStatus,
        session::{EntryKind, ModelPurpose, TitleSource, Truncation, event::ResponseOutcome},
    };
    use skyhook_media::ImageFormat;
    use skyhook_tools::diagnostic::{CauseKind, ConnectionFault, RequestEffects, SiteKind};
    use skyhook_worker::diagnostic::{Effects, Operation, PathRole, SubjectKind};
    use skyhook_worker::{op::IoKind, secret::CredentialKind};
    capabilities(db, seeding)?;
    dictionaries.flagged("entry_kind", ["activity"], &EntryKind::ALL, |kind| {
        [kind.is_activity()]
    })?;
    dictionaries.names("title_source", &TitleSource::ALL)?;
    dictionaries.names("target_source", &TargetSource::ALL)?;
    dictionaries.names("transport", &ConnectionKind::ALL)?;
    dictionaries.names("ssh_auth", &SshAuthKind::ALL)?;
    dictionaries.names("winrm_auth", &skyhook_worker::origin::winrm::AuthKind::ALL)?;
    dictionaries.names("winrm_tls", &skyhook_worker::origin::winrm::TlsKind::ALL)?;
    dictionaries.names("os", &skyhook_worker::target::Os::ALL)?;
    dictionaries.names("credential_source", &CredentialKind::ALL)?;
    dictionaries.names("state_mode", &StateMode::ALL)?;
    dictionaries.names("reasoning_effort", &Effort::ALL)?;
    dictionaries.names("model_purpose", &ModelPurpose::ALL)?;
    dictionaries.names("message_role", &crate::session::MessageRole::ALL)?;
    dictionaries.names("image_format", &ImageFormat::ALL)?;
    dictionaries.names("user_part_kind", &crate::session::UserPartKind::ALL)?;
    dictionaries.names("job_event_kind", &crate::session::JobEventKind::ALL)?;
    dictionaries.names("item_kind", &ItemKind::ALL)?;
    dictionaries.names("replay_binding", &Binding::ALL)?;
    dictionaries.names("replay_format", &ReplayFormat::ALL)?;
    dictionaries.names("todo_status", &TodoStatus::ALL)?;
    dictionaries.names("history_lifetime", &HistoryLifetime::ALL)?;
    dictionaries.names("response_outcome", &ResponseOutcome::ALL)?;
    dictionaries.names("cut_reason", &Truncation::ALL)?;
    dictionaries.names("job_role", &JobRole::ALL)?;
    dictionaries.names("capture_kind", &skyhook_tools::output::CaptureKind::ALL)?;
    dictionaries.names(
        "capture_detection",
        &skyhook_tools::output::json::Detection::ALL,
    )?;
    dictionaries.names(
        "field_presentation",
        &skyhook_tools::output::FieldPresentation::ALL,
    )?;
    dictionaries.names("diagnostic_operation", &Operation::ALL)?;
    dictionaries.names("diagnostic_subject", &SubjectKind::ALL)?;
    dictionaries.names("diagnostic_site", &SiteKind::ALL)?;
    dictionaries.names("diagnostic_effects", &Effects::ALL)?;
    dictionaries.names("diagnostic_request_effects", &RequestEffects::ALL)?;
    dictionaries.names("diagnostic_cause", &CauseKind::ALL)?;
    dictionaries.names("diagnostic_connection_fault", &ConnectionFault::ALL)?;
    dictionaries.names("diagnostic_io_kind", &IoKind::ALL)?;
    dictionaries.names("diagnostic_path_role", &PathRole::ALL)?;
    dictionaries.flagged("job_state", ["terminal"], &JobState::ALL, |state| {
        [state.is_terminal()]
    })?;
    dictionaries.names("job_answer", &AnswerKind::ALL)?;
    dictionaries.names("request_phase", &crate::session::requests::PhaseKind::ALL)?;
    dictionaries.names("agent_last", &crate::session::agent_state::LastKind::ALL)?;
    dictionaries.names(
        "agent_outcome",
        &crate::session::agent_state::OutcomeKind::ALL,
    )?;
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

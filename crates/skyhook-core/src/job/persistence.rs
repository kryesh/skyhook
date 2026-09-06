use crate::session::{
    EventRecord, SessionError, SessionEvent, SessionStore, is_safe_artifact_path,
};

use super::{DeliveryState, JobEntry, JobError, JobManager, JobOutcome, JobSpec};

pub(super) async fn restore(
    store: SessionStore,
    records: &[EventRecord],
) -> Result<JobManager, JobError> {
    let mut jobs = std::collections::HashMap::new();
    let mut maximum = 0_u64;
    for record in records {
        match &record.event {
            SessionEvent::JobCreated {
                origin,
                job,
                parent,
                tool,
                name,
                accepts_input,
                background,
                location,
                output_schema,
                ..
            } => {
                maximum = maximum.max(job.get());
                let (entry, _receiver) = JobEntry::new(
                    JobSpec {
                        origin: origin.clone(),
                        agent: record.agent.clone(),
                        parent: *parent,
                        tool: tool.clone(),
                        name: name.clone(),
                        arguments: serde_json::Value::Null,
                        output_schema: output_schema.clone(),
                        accepts_input: *accepts_input,
                        background: *background,
                        authorization_scope: None,
                        location: location.clone(),
                    },
                    record.timestamp_millis,
                );
                jobs.insert(*job, entry);
            }
            SessionEvent::AgentStarted {
                owner_job: Some(job),
                location,
                ..
            } => {
                if let Some(entry) = jobs.get_mut(job) {
                    entry.location.clone_from(location);
                }
            }
            SessionEvent::JobStateChanged { job, state } => {
                if let Some(entry) = jobs.get_mut(job) {
                    entry.state = *state;
                }
            }
            SessionEvent::JobFinished {
                job,
                state,
                output_path,
                error,
                images,
                console_output,
                denial,
            } => {
                if let Some(entry) = jobs.get_mut(job) {
                    entry.state = *state;
                    entry.error.clone_from(error);
                    entry.denial.clone_from(denial);
                    entry.images.clone_from(images);
                    entry.console_output.clone_from(console_output);
                    if let Some(relative) = output_path {
                        if !is_safe_artifact_path(relative) {
                            return Err(SessionError::UnsafeArtifactPath.into());
                        }
                        if !store.directory().join(relative).is_file() {
                            return Err(SessionError::from(std::io::Error::new(
                                std::io::ErrorKind::NotFound,
                                "missing job output artifact",
                            ))
                            .into());
                        }
                    }
                }
            }
            SessionEvent::JobClaimed { job } => {
                if let Some(entry) = jobs.get_mut(job) {
                    entry.delivery = DeliveryState::Claimed;
                }
            }
            SessionEvent::JobInjected { job } => {
                if let Some(entry) = jobs.get_mut(job) {
                    entry.delivery = DeliveryState::Injected;
                }
            }
            _ => {}
        }
    }
    let active = jobs
        .iter()
        .filter_map(|(job, entry)| (!entry.state.is_terminal()).then_some(*job))
        .collect::<Vec<_>>();
    let manager = JobManager::with_jobs(store, jobs, maximum.saturating_add(1).max(1));
    for job in active {
        manager.finish(job, JobOutcome::Interrupted).await?;
    }
    Ok(manager)
}

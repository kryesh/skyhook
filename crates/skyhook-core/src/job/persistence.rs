use crate::{
    identity::{AgentId, JobId},
    provider::protocol::{Message, UserContent},
};

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
                    if entry.state == super::JobState::Completed
                        && *state == super::JobState::Running
                    {
                        entry.output = None;
                        entry.images.clear();
                        entry.error = None;
                        entry.denial = None;
                        entry.delivery = DeliveryState::Pending;
                        entry.background = true;
                    }
                    if *state == super::JobState::WaitingInput
                        || (entry.state == super::JobState::WaitingInput
                            && *state == super::JobState::Running)
                    {
                        entry.output = None;
                        entry.delivery = DeliveryState::Pending;
                        entry.background = true;
                    }
                    entry.state = *state;
                }
            }
            SessionEvent::MessageCommitted { message } => {
                acknowledge_message(&mut jobs, &record.agent, message);
            }
            SessionEvent::JobFinished {
                job,
                state,
                output_path,
                error,
                images,
                denial,
            } => {
                if let Some(entry) = jobs.get_mut(job) {
                    entry.state = *state;
                    entry.delivery = DeliveryState::Pending;
                    entry.error.clone_from(error);
                    entry.denial.clone_from(denial);
                    entry.images.clone_from(images);
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
    manager.inner.progress.lock().await.project(records);
    for job in active {
        manager.finish(job, JobOutcome::Interrupted).await?;
    }
    Ok(manager)
}

/// The committed host-generated notification is the delivery acknowledgement.
/// Infer it in journal order, so a later retained resume resets delivery normally.
/// User text/parent input is deliberately not parsed as a host notification.
pub(super) fn acknowledge_message(
    jobs: &mut std::collections::HashMap<JobId, JobEntry>,
    owner: &AgentId,
    message: &Message,
) {
    let Message::User(content) = message else {
        return;
    };
    for block in content {
        let UserContent::Runtime { text } = block else {
            continue;
        };
        let Some(json) = text
            .strip_prefix("<skyhook_job_events>\n")
            .and_then(|text| text.strip_suffix("\n</skyhook_job_events>"))
        else {
            continue;
        };
        let Ok(envelopes) = serde_json::from_str::<Vec<serde_json::Value>>(json) else {
            continue;
        };
        for envelope in envelopes {
            let Some(id) = envelope.get("id") else {
                continue;
            };
            let Some(state) = envelope.get("state") else {
                continue;
            };
            let (Ok(id), Ok(state)) = (
                serde_json::from_value::<JobId>(id.clone()),
                serde_json::from_value::<super::JobState>(state.clone()),
            ) else {
                continue;
            };
            let Some(entry) = jobs.get_mut(&id) else {
                continue;
            };
            if &entry.agent == owner
                && entry.background
                && entry.state.presented() == state
                && entry.deliverable()
            {
                entry.reserve_delivery(DeliveryState::Injected);
            }
        }
    }
}

use std::sync::Arc;

use tokio::{
    fs,
    sync::{Notify, mpsc},
};

use crate::session::{
    EventRecord, SessionError, SessionEvent, SessionStore, is_safe_artifact_path,
};

use super::{CancellationToken, JOB_INPUT_CAPACITY, JobEntry, JobError, JobManager, JobState};

pub(super) async fn restore(
    store: SessionStore,
    records: &[EventRecord],
) -> Result<JobManager, JobError> {
    let mut jobs = std::collections::HashMap::new();
    let mut maximum = 0_u64;
    for record in records {
        match &record.event {
            SessionEvent::JobCreated {
                job,
                parent,
                tool,
                accepts_input,
                background,
                location,
                ..
            } => {
                maximum = maximum.max(job.get());
                let (input, _receiver) = mpsc::channel(JOB_INPUT_CAPACITY);
                jobs.insert(
                    *job,
                    JobEntry {
                        agent: record.agent.clone(),
                        parent: *parent,
                        tool: tool.clone(),
                        state: JobState::Queued,
                        output: None,
                        images: Vec::new(),
                        error: None,
                        accepts_input: *accepts_input,
                        input,
                        cancellation: CancellationToken::new(),
                        notify: Arc::new(Notify::new()),
                        operation: Arc::new(tokio::sync::Mutex::new(())),
                        task_abort: None,
                        cancellation_watchdog_started: false,
                        next_progress: 1,
                        claimed: false,
                        injected: false,
                        background: *background,
                        authorization_scope: None,
                        location: location.clone(),
                    },
                );
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
            } => {
                if let Some(entry) = jobs.get_mut(job) {
                    entry.state = *state;
                    entry.error.clone_from(error);
                    entry.images.clone_from(images);
                    if let Some(relative) = output_path {
                        if !is_safe_artifact_path(relative) {
                            return Err(SessionError::UnsafeArtifactPath.into());
                        }
                        let bytes = fs::read(store.directory().join(relative))
                            .await
                            .map_err(SessionError::from)?;
                        entry.output =
                            Some(serde_json::from_slice(&bytes).map_err(SessionError::from)?);
                    }
                }
            }
            SessionEvent::JobClaimed { job } => {
                if let Some(entry) = jobs.get_mut(job) {
                    entry.claimed = true;
                }
            }
            SessionEvent::JobInjected { job } => {
                if let Some(entry) = jobs.get_mut(job) {
                    entry.injected = true;
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
        manager
            .finish(
                job,
                Err("interrupted while the session was not running".to_owned()),
                Some(JobState::Interrupted),
            )
            .await?;
    }
    Ok(manager)
}

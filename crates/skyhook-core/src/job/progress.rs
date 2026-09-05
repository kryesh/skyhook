use chrono::Utc;
use serde_json::Value;

use crate::identity::JobId;

use super::{JobError, JobManager, JobProgressRecord};

pub(super) async fn publish(
    manager: &JobManager,
    id: JobId,
    kind: String,
    data: Value,
) -> Result<(), JobError> {
    let operation = manager.operation(id).await?;
    let _operation = operation.lock().await;
    let record = {
        let jobs = manager.inner.jobs.lock().await;
        let entry = jobs.get(&id).ok_or(JobError::Unknown(id))?;
        if entry.state.is_terminal() {
            return Err(JobError::AlreadyTerminal(id));
        }
        JobProgressRecord {
            sequence: entry.next_progress,
            timestamp_millis: Utc::now().timestamp_millis(),
            kind,
            data,
        }
    };
    manager.inner.store.append_job_event(id, &record).await?;
    let mut jobs = manager.inner.jobs.lock().await;
    let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
    entry.next_progress = record.sequence.saturating_add(1);
    Ok(())
}

pub(super) async fn events(
    manager: &JobManager,
    id: JobId,
    after: u64,
    limit: usize,
) -> Result<Vec<JobProgressRecord>, JobError> {
    if !manager.inner.jobs.lock().await.contains_key(&id) {
        return Err(JobError::Unknown(id));
    }
    Ok(manager
        .inner
        .store
        .read_job_events(id, after, limit)
        .await?)
}

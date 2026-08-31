use std::sync::Arc;

use chrono::Utc;
use serde_json::Value;

use crate::{
    identity::JobId,
    tool::{ProgressFuture, ProgressSink, ToolError},
};

use super::{JobError, JobManager, JobProgressRecord};

pub(super) async fn publish(
    manager: &JobManager,
    id: JobId,
    kind: String,
    data: Value,
) -> Result<(), JobError> {
    let record = {
        let mut jobs = manager.inner.jobs.lock().await;
        let entry = jobs.get_mut(&id).ok_or(JobError::Unknown(id))?;
        if entry.state.is_terminal() {
            return Err(JobError::AlreadyTerminal(id));
        }
        let record = JobProgressRecord {
            sequence: entry.next_progress,
            timestamp_millis: Utc::now().timestamp_millis(),
            kind,
            data,
        };
        entry.next_progress = entry.next_progress.saturating_add(1);
        record
    };
    manager.inner.store.append_job_event(id, &record).await?;
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

pub(super) fn sink(manager: JobManager, id: JobId) -> Arc<dyn ProgressSink> {
    Arc::new(JobProgressSink { manager, id })
}

struct JobProgressSink {
    manager: JobManager,
    id: JobId,
}

impl ProgressSink for JobProgressSink {
    fn publish(&self, kind: String, data: Value) -> ProgressFuture {
        let manager = self.manager.clone();
        let id = self.id;
        Box::pin(async move {
            publish(&manager, id, kind, data)
                .await
                .map_err(|error| ToolError::Failed(error.to_string()))
        })
    }
}

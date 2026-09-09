//! Agent-facing event waiting, separate from saved-output inspection.

use super::{AgentCommand, SessionRuntime};
use crate::{
    provider::protocol::UserContent,
    tool::{ToolContext, ToolError},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{mpsc, watch};

/// Input and background notifications use the same per-agent delivery gate.
#[derive(Clone)]
pub(super) struct AgentSender {
    sender: mpsc::Sender<AgentCommand>,
    wake: Arc<AgentWake>,
}

struct AgentWake {
    revision: watch::Sender<u64>,
    batch: std::sync::Mutex<EventBatch>,
    ready_input_revision: AtomicU64,
    observed_input: AtomicU64,
    observed: AtomicU64,
}

/// Messages and lifecycle notifications share one durable delivery receipt.
/// Preparing/presenting a batch is cancellable; its parent-history commit and
/// acknowledgement run to completion even if the caller is interrupted.
pub(super) struct PendingEventBatch {
    jobs: Option<crate::job::PendingDelivery>,
}

impl PendingEventBatch {
    pub(super) async fn commit(
        self,
        runtime: &SessionRuntime,
        agent: &crate::identity::AgentId,
        message: crate::provider::protocol::Message,
    ) -> Result<u64, super::HarnessError> {
        let store = runtime.store.clone();
        let agent = agent.clone();
        tokio::spawn(async move {
            match self.jobs {
                Some(jobs) => jobs
                    .commit(message)
                    .await
                    .map_err(super::HarnessError::from),
                None => Ok(store
                    .append(
                        agent,
                        crate::session::SessionEvent::MessageCommitted { message },
                    )
                    .await?
                    .sequence),
            }
        })
        .await
        .map_err(|error| crate::session::SessionError::Io(std::io::Error::other(error)))?
    }
}

#[derive(Default)]
struct EventBatch {
    revision: u64,
    input_revision: u64,
    scheduled: bool,
}

impl AgentSender {
    pub(super) fn new(sender: mpsc::Sender<AgentCommand>) -> Self {
        Self {
            sender,
            wake: Arc::new(AgentWake {
                revision: watch::channel(0).0,
                batch: std::sync::Mutex::new(EventBatch::default()),
                ready_input_revision: AtomicU64::new(0),
                observed_input: AtomicU64::new(0),
                observed: AtomicU64::new(0),
            }),
        }
    }

    pub(super) async fn send(
        &self,
        command: AgentCommand,
    ) -> Result<(), mpsc::error::SendError<AgentCommand>> {
        let notify = !matches!(command, AgentCommand::JobsReady);
        let permit = match self.sender.reserve().await {
            Ok(permit) => permit,
            Err(_) => return Err(mpsc::error::SendError(command)),
        };
        // Schedule before publication; no await separates these operations.
        if notify {
            self.schedule(true);
        }
        permit.send(command);
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn capacity(&self) -> usize {
        self.sender.capacity()
    }

    #[cfg(test)]
    pub(super) async fn closed(&self) {
        self.sender.closed().await
    }

    pub(super) fn jobs_ready(&self) {
        self.schedule(false);
        // A full mailbox already guarantees another request boundary. Never
        // stall notification of other agents behind this one's mailbox.
        let _ = self.sender.try_send(AgentCommand::JobsReady);
    }

    /// One bounded coalescing window per agent, shared by every delivery path.
    /// Continuous activity cannot postpone an already scheduled batch.
    fn schedule(&self, input: bool) {
        let mut batch = self
            .wake
            .batch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        batch.revision = batch.revision.wrapping_add(1);
        if input {
            batch.input_revision = batch.input_revision.wrapping_add(1);
        }
        if batch.scheduled {
            return;
        }
        batch.scheduled = true;
        let wake = Arc::downgrade(&self.wake);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let Some(wake) = wake.upgrade() else { return };
            let mut batch = wake
                .batch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            wake.ready_input_revision
                .store(batch.input_revision, Ordering::Release);
            batch.scheduled = false;
            // Publish under the lock before another window can begin.
            wake.revision.send_replace(batch.revision);
        });
    }

    pub(super) async fn flush_events(
        &self,
        cancellation: &crate::job::CancellationToken,
    ) -> Result<(), super::HarnessError> {
        let mut revision = self.wake.revision.subscribe();
        let requested = self
            .wake
            .batch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .revision;
        loop {
            if cancellation.is_cancelled() {
                return Err(super::HarnessError::Interrupted);
            }
            if *revision.borrow_and_update() >= requested {
                return Ok(());
            }
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(super::HarnessError::Interrupted),
                result = revision.changed() => {
                    if result.is_err() { return Err(super::HarnessError::Interrupted); }
                }
            }
        }
    }

    pub(super) fn begin_request(&self) {
        self.wake
            .observed
            .store(*self.wake.revision.borrow(), Ordering::Release);
        self.wake.observed_input.store(
            self.wake.ready_input_revision.load(Ordering::Acquire),
            Ordering::Release,
        );
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct WaitArgs {
    /// Maximum seconds to wait, as a positive integer. Omitted/null waits indefinitely.
    #[schemars(range(min = 1))]
    pub(super) timeout: Option<u64>,
}

#[derive(Serialize, JsonSchema, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum WakeReason {
    Event,
    Timeout,
}

#[derive(Serialize, JsonSchema, Debug, PartialEq)]
pub(super) struct WaitOutput {
    pub(super) reason: WakeReason,
}

impl SessionRuntime {
    pub(super) async fn wait_for_event(
        &self,
        context: &ToolContext,
        args: WaitArgs,
    ) -> Result<WaitOutput, ToolError> {
        let deadline = args
            .timeout
            .map(|seconds| {
                if seconds == 0 {
                    return Err(ToolError::InvalidArguments(
                        "timeout must be a positive integer".into(),
                    ));
                }
                tokio::time::Instant::now()
                    .checked_add(std::time::Duration::from_secs(seconds))
                    .ok_or_else(|| ToolError::InvalidArguments("timeout is too large".into()))
            })
            .transpose()?;
        let sender = self
            .agent_sender(&context.agent)
            .ok_or_else(|| ToolError::Failed("calling agent is not active".into()))?;
        let mut revision = sender.wake.revision.subscribe();
        loop {
            if context.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            // Only released batches are visible. A stale completion signal
            // must not count as an event if its output was already claimed.
            let released = *revision.borrow_and_update();
            if sender.wake.ready_input_revision.load(Ordering::Acquire)
                != sender.wake.observed_input.load(Ordering::Acquire)
                || (released != sender.wake.observed.load(Ordering::Acquire)
                    && self.jobs.has_pending(&context.agent).await)
            {
                return Ok(WaitOutput {
                    reason: WakeReason::Event,
                });
            }
            tokio::select! {
                biased;
                () = context.cancelled() => return Err(ToolError::Cancelled),
                changed = revision.changed() => {
                    if changed.is_err() { return Err(ToolError::Cancelled); }
                }
                () = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => return Ok(WaitOutput { reason: WakeReason::Timeout }),
            }
        }
    }

    pub(super) async fn pending_event_content(
        &self,
        agent: &crate::identity::AgentId,
        capabilities: &crate::tool::policy::CapabilitySet,
        location: &crate::execution::ExecutionLocation,
    ) -> Result<(Vec<UserContent>, PendingEventBatch), super::HarnessError> {
        let pending = self.jobs.pending_delivery(agent).await?;
        let content = self
            .job_event_content(&pending, capabilities, location)
            .await;
        // Never hold an empty receipt's delivery gate across a model request.
        let jobs = (!content.is_empty()).then_some(pending);
        Ok((content, PendingEventBatch { jobs }))
    }

    async fn job_event_content(
        &self,
        pending: &crate::job::PendingDelivery,
        capabilities: &crate::tool::policy::CapabilitySet,
        location: &crate::execution::ExecutionLocation,
    ) -> Vec<UserContent> {
        let mut presented = Vec::new();
        for message in pending.messages() {
            let mut event = serde_json::to_value(message).expect("child messages serialize");
            event["kind"] = serde_json::json!("message");
            presented.push(event);
        }
        for job in pending.envelopes() {
            // The independently delivered last reply is the completion payload.
            // Present only metadata here: inspecting the saved output would also
            // reintroduce large replies through preview/truncation fields.
            if job.tool == "agent"
                && job.state == crate::job::JobState::Completed
                && let Ok(Some(sequence)) = self.jobs.last_agent_message(job.id).await
            {
                let mut metadata = job.clone();
                metadata.output = None;
                let mut view = metadata
                    .presented_for(capabilities, Some(location), true)
                    .expect("job metadata serializes");
                view["last_message"] = serde_json::json!(sequence);
                presented.push(view);
                continue;
            }
            match self
                .jobs
                .inspect_output_for(
                    crate::job::output::OutputArgs::new(job.id),
                    capabilities,
                    location,
                )
                .await
            {
                Ok(view) => presented.push(view),
                Err(error) => presented.push(
                    serde_json::json!({"id":job.id,"state":job.state,"error":error.to_string()}),
                ),
            }
        }
        if presented.is_empty() {
            return Vec::new();
        }
        vec![UserContent::Runtime {
            text: format!(
                "<skyhook_job_events>\n{}\n</skyhook_job_events>",
                serde_json::to_string(&presented).unwrap_or_else(|_| "[]".to_owned())
            ),
        }]
    }
}

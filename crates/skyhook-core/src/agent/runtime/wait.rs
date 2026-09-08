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
    child_messages: std::sync::Mutex<Vec<ChildMessage>>,
    ready_input_revision: AtomicU64,
    observed_input: AtomicU64,
    observed: AtomicU64,
}

#[derive(Serialize)]
pub(super) struct ChildMessage {
    pub id: crate::identity::JobId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub message: u64,
    pub text: String,
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
                child_messages: std::sync::Mutex::new(Vec::new()),
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

    pub(super) fn child_message(&self, message: ChildMessage) {
        // Keep payloads outside the bounded command mailbox. A foreground child
        // must not block on its parent draining progress while awaiting that child.
        self.wake
            .child_messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(message);
        self.jobs_ready();
    }

    pub(super) fn has_child_messages(&self) -> bool {
        !self
            .wake
            .child_messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    fn take_child_messages(&self) -> Vec<ChildMessage> {
        std::mem::take(
            &mut *self
                .wake
                .child_messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
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
                    && (sender.has_child_messages() || self.jobs.has_pending(&context.agent).await))
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
    ) -> Result<Vec<UserContent>, super::HarnessError> {
        let pending = self.jobs.take_pending(agent).await?;
        // Reserve completions before draining messages: every intermediate reply
        // from a completed child was queued before its completion was published.
        let mut content = self.child_message_content(agent);
        if !pending.is_empty() {
            content.extend(
                self.job_event_content(&pending, capabilities, location)
                    .await,
            );
        }
        Ok(content)
    }

    pub(super) fn child_message_content(
        &self,
        agent: &crate::identity::AgentId,
    ) -> Vec<UserContent> {
        let messages = self
            .agent_sender(agent)
            .map(|sender| sender.take_child_messages())
            .unwrap_or_default();
        if messages.is_empty() {
            return Vec::new();
        }
        vec![UserContent::Runtime {
            text: format!(
                "<skyhook_agent_messages>\n{}\n</skyhook_agent_messages>",
                serde_json::to_string(&messages).expect("child messages serialize")
            ),
        }]
    }

    async fn job_event_content(
        &self,
        pending: &[crate::job::JobEnvelope],
        capabilities: &crate::tool::policy::CapabilitySet,
        location: &crate::execution::ExecutionLocation,
    ) -> Vec<UserContent> {
        let mut presented = Vec::new();
        for job in pending {
            match self
                .jobs
                .present_output_for(
                    crate::job::output::OutputArgs::new(job.id),
                    capabilities,
                    location,
                    true,
                )
                .await
            {
                Ok(view) => presented.push(view),
                Err(error) => presented.push(
                    serde_json::json!({"id":job.id,"state":job.state,"error":error.to_string()}),
                ),
            }
        }
        vec![UserContent::Runtime {
            text: format!(
                "<skyhook_job_events>\n{}\n</skyhook_job_events>",
                serde_json::to_string(&presented).unwrap_or_else(|_| "[]".to_owned())
            ),
        }]
    }
}

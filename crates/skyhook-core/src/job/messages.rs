//! Child replies are derived from their source history records, independently of
//! lifecycle delivery. There is deliberately no second message-publication event.
use super::*;
use crate::provider::protocol::BlockContent;

const MESSAGE_BATCH_COUNT: usize = 128;

pub(super) fn visible_text(message: &Message) -> Option<String> {
    let Message::Assistant(items) = message else {
        return None;
    };
    Some(
        items
            .iter()
            .flat_map(|item| &item.blocks)
            .filter_map(|block| match &block.content {
                BlockContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect(),
    )
}

impl JobEntry {
    pub(super) fn has_pending(&self) -> bool {
        !self.messages.is_empty()
            || (self.background && self.deliverable() && self.delivery == DeliveryState::Pending)
    }

    pub(super) fn publish_message(&mut self, id: JobId, sequence: u64, text: String) {
        if text.is_empty() {
            return;
        }
        self.last_agent_message = Some(sequence);
        self.messages.push(AgentMessage {
            id,
            name: self.name.clone(),
            message: sequence,
            text,
        });
    }
}

impl JobManager {
    /// Commit a child's assistant history and publish its visible text as one
    /// cancellation-shielded operation. The source sequence is the delivery ID.
    /// Empty visible text is committed to history but produces no delivery/wake.
    pub(crate) async fn commit_child_message(
        &self,
        child: &AgentId,
        job: JobId,
        message: Message,
        text: String,
    ) -> Result<u64, JobError> {
        // Refuse a projection that replay could not reproduce (including reasoning).
        if visible_text(&message).as_deref() != Some(text.as_str()) {
            return Err(JobError::Internal(
                "child message text does not match committed assistant text".into(),
            ));
        }
        let manager = self.clone();
        let child = child.clone();
        tokio::spawn(async move {
            let _delivery = manager.inner.delivery_operation.lock().await;
            let (owner, associated) = {
                let jobs = manager.inner.jobs.lock().await;
                let entry = jobs.get(&job).ok_or(JobError::Unknown(job))?;
                (entry.agent.clone(), entry.child.clone())
            };
            if child.parent().as_ref() != Some(&owner) {
                return Err(JobError::Internal("child job owner mismatch".into()));
            }
            let valid = if let Some(associated) = associated {
                associated == child
            } else {
                let mut valid = false;
                manager
                    .inner
                    .store
                    .visit_records_after(0, |records| {
                        valid = records.iter().any(|record| {
                            record.agent == child
                                && matches!(&record.event, SessionEvent::AgentStarted {
                                parent: Some(parent), owner_job: Some(owner_job), ..
                            } if parent == &owner && *owner_job == job)
                        });
                    })
                    .await;
                valid
            };
            if !valid {
                return Err(JobError::Internal(
                    "child job association missing or mismatched".into(),
                ));
            }
            let record = manager
                .inner
                .store
                .append(child.clone(), SessionEvent::MessageCommitted { message })
                .await?;
            let mut jobs = manager.inner.jobs.lock().await;
            let entry = jobs.get_mut(&job).ok_or(JobError::Unknown(job))?;
            entry.child = Some(child);
            let visible = !text.is_empty();
            entry.publish_message(job, record.sequence, text);
            if visible {
                // Foreground child replies are just as deliverable as background ones.
                let _ = manager
                    .inner
                    .completions
                    .send(JobCompletion { agent: owner, job });
            }
            Ok(record.sequence)
        })
        .await
        .map_err(|error| JobError::Internal(error.to_string()))?
    }

    /// Last committed *visible* child message, even after acknowledgement/resume.
    pub(crate) async fn last_agent_message(&self, job: JobId) -> Result<Option<u64>, JobError> {
        let jobs = self.inner.jobs.lock().await;
        Ok(jobs
            .get(&job)
            .ok_or(JobError::Unknown(job))?
            .last_agent_message)
    }
}

fn message_size(message: &AgentMessage) -> usize {
    // Include the runtime kind discriminator and JSON array separator.
    serde_json::to_vec(message).map_or(DELIVERY_BATCH_BYTES, |bytes| bytes.len().saturating_add(18))
}

pub(super) fn batch_size(messages: &[AgentMessage]) -> usize {
    messages.iter().fold(0_usize, |bytes, message| {
        bytes.saturating_add(message_size(message))
    })
}

pub(super) fn pending_messages(
    jobs: &HashMap<JobId, JobEntry>,
    owner: &AgentId,
) -> Vec<AgentMessage> {
    let mut messages: Vec<_> = jobs
        .values()
        .filter(|entry| &entry.agent == owner)
        .flat_map(|entry| &entry.messages)
        .collect();
    messages.sort_by_key(|message| message.message);
    let mut budget: usize = 0;
    let mut pending = Vec::new();
    for message in messages.into_iter().take(MESSAGE_BATCH_COUNT) {
        let cost = message_size(message);
        if !pending.is_empty() && budget.saturating_add(cost) > DELIVERY_BATCH_BYTES {
            break;
        }
        // Like lifecycle output, always allow one oversized item to make progress.
        budget = budget.saturating_add(cost);
        pending.push(message.clone());
    }
    pending
}

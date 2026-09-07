//! Request-boundary user input mailbox and cancellation ownership.

use std::sync::atomic::{AtomicU8, Ordering};

use super::*;

const PENDING: u8 = 0;
const CLAIMED: u8 = 1;
const CANCELLED: u8 = 2;

/// Cancellation/ownership token for one request-boundary user submission.
///
/// Clone the token before passing it to `enqueue_prompt_with_options`. A
/// successful `cancel` guarantees the input will never enter model history. Once
/// claimed, only the enqueue commit receipt can determine whether it committed;
/// do not edit or resubmit it until that receipt resolves. Use a fresh token for
/// each submission (including edits and retries).
#[derive(Clone, Debug, Default)]
pub struct QueuedPromptToken(Arc<AtomicU8>);

impl QueuedPromptToken {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancel unless the runtime has already claimed this input for commit.
    /// Returns true for an input that was already cancelled, too.
    #[must_use]
    pub fn cancel(&self) -> bool {
        match self
            .0
            .compare_exchange(PENDING, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) | Err(CANCELLED) => true,
            Err(_) => false,
        }
    }

    #[must_use]
    pub fn is_claimed(&self) -> bool {
        self.0.load(Ordering::Acquire) == CLAIMED
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire) == CANCELLED
    }

    fn claim(&self) -> bool {
        self.0
            .compare_exchange(PENDING, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

/// One submission in an atomic request-boundary batch.
#[derive(Clone, Debug)]
pub struct QueuedPrompt {
    pub text: String,
    pub paths: Vec<PathBuf>,
    pub options: PromptOptions,
    pub token: QueuedPromptToken,
}

pub(super) struct QueuedInput {
    pub content: Vec<UserContent>,
    pub model: Option<String>,
    pub token: QueuedPromptToken,
    pub committed: oneshot::Sender<Result<(), HarnessError>>,
}

/// Reject unclaimed inputs on interrupt without discarding ordinary commands or
/// job notifications. The caller receives an error and retains the editable row.
pub(super) fn reject_pending(
    rx: &mut mpsc::Receiver<AgentCommand>,
    deferred: &mut VecDeque<AgentCommand>,
) {
    let count = rx.len();
    for _ in 0..count {
        let Ok(command) = rx.try_recv() else { break };
        deferred.push_back(command);
    }
    deferred.retain_mut(|command| {
        if let AgentCommand::QueuedInputs(inputs) = command {
            for input in inputs.drain(..) {
                let _ = input.token.cancel();
                let _ = input.committed.send(Err(HarnessError::Interrupted));
            }
            false
        } else {
            true
        }
    });
}

impl SessionRuntime {
    pub(super) async fn select_model(
        &self,
        agent: &AgentId,
        context: &mut AgentContext,
        model_profile: &mut String,
        capabilities: &CapabilitySet,
        model: String,
    ) -> Result<(), HarnessError> {
        if model == *model_profile {
            return Ok(());
        }
        let selected = self
            .harness
            .model_profiles
            .get(&model)
            .cloned()
            .ok_or_else(|| HarnessError::UnknownModelProfile(model.clone()))?;
        let replacement = if selected != context.profile {
            let replacement = self
                .open_agent_context(
                    agent,
                    selected.clone(),
                    context.template.system.clone(),
                    capabilities,
                    false,
                )
                .await?;
            if !selected.supports_images && replacement.contains_images() {
                return Err(HarnessError::ImagesUnsupported(selected.model.clone()));
            }
            Some(replacement)
        } else {
            None
        };
        self.store
            .append(
                agent.clone(),
                SessionEvent::ModelChanged {
                    model_profile: model.clone(),
                    max_context: selected.max_context,
                },
            )
            .await?;
        if let Some(replacement) = replacement {
            *context = replacement;
        }
        *model_profile = model;
        if let Some(live) = self
            .agents
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(agent)
        {
            live.model_profile.clone_from(model_profile);
        }
        Ok(())
    }

    pub(super) async fn consume_queued_input(
        &self,
        agent: &AgentId,
        context: &mut AgentContext,
        model_profile: &mut String,
        capabilities: &CapabilitySet,
        input: QueuedInput,
    ) -> bool {
        if self.shutting_down.load(Ordering::Acquire) {
            let _ = input.token.cancel();
            let _ = input.committed.send(Err(HarnessError::AgentStopped));
            return false;
        }
        if !input.token.claim() {
            let _ = input.committed.send(Err(HarnessError::Interrupted));
            return false;
        }
        let result = async {
            let profile = match &input.model {
                Some(model) => self
                    .harness
                    .model_profiles
                    .get(model)
                    .ok_or_else(|| HarnessError::UnknownModelProfile(model.clone()))?,
                None => &context.profile,
            };
            if !profile.supports_images
                && input
                    .content
                    .iter()
                    .any(|content| matches!(content, UserContent::Image { .. }))
            {
                return Err(HarnessError::ImagesUnsupported(profile.model.clone()));
            }
            if let Some(model) = input.model {
                self.select_model(agent, context, model_profile, capabilities, model)
                    .await?;
            }
            let message = Message::User(input.content);
            let origin = self.commit(agent, message.clone()).await?;
            context.projected.push((origin, message));
            Ok(())
        }
        .await;
        let consumed = result.is_ok();
        if consumed {
            // Receipt readers must already observe a busy agent when this input
            // starts a fresh turn; publishing afterward races the UI's idle check.
            self.activity(agent, AgentActivity::Working);
        }
        let _ = input.committed.send(result);
        consumed
    }

    /// Consume every member before allowing a provider request, even if commits
    /// yield. Interrupts reject the unclaimed remainder with individual receipts.
    pub(super) async fn consume_queued_batch(
        &self,
        agent: &AgentId,
        context: &mut AgentContext,
        model_profile: &mut String,
        capabilities: &CapabilitySet,
        cancellation: &CancellationToken,
        inputs: Vec<QueuedInput>,
    ) -> bool {
        let mut consumed = false;
        for input in inputs {
            if cancellation.is_cancelled() {
                let _ = input.token.cancel();
                let _ = input.committed.send(Err(HarnessError::Interrupted));
            } else {
                consumed |= self
                    .consume_queued_input(agent, context, model_profile, capabilities, input)
                    .await;
            }
        }
        consumed
    }

    /// Drain a bounded snapshot of the mailbox, preserving all non-queue commands
    /// for the outer loop (notably JobsReady and ordinary prompt completion).
    pub(super) async fn consume_queued_inputs(
        &self,
        turn: &TurnContext<'_>,
        context: &mut AgentContext,
        model_profile: &mut String,
        rx: &mut mpsc::Receiver<AgentCommand>,
        deferred: &mut VecDeque<AgentCommand>,
    ) -> bool {
        let mut consumed = false;
        let count = rx.len();
        for _ in 0..count {
            if turn.cancellation.is_cancelled() {
                break;
            }
            let Ok(command) = rx.try_recv() else { break };
            match command {
                AgentCommand::QueuedInputs(inputs) => {
                    consumed |= self
                        .consume_queued_batch(
                            turn.agent,
                            context,
                            model_profile,
                            turn.capabilities,
                            turn.cancellation,
                            inputs,
                        )
                        .await;
                }
                command => deferred.push_back(command),
            }
        }
        consumed
    }
}

#[cfg(test)]
mod tests {
    use super::QueuedPromptToken;

    #[test]
    fn cancellation_and_claim_are_exclusive_across_clones() {
        let pending = QueuedPromptToken::new();
        let cancelled = pending.clone();
        assert!(cancelled.cancel());
        assert!(pending.cancel());
        assert!(!pending.claim());
        assert!(!pending.is_claimed());

        let claimed = QueuedPromptToken::new();
        let copy = claimed.clone();
        assert!(claimed.claim());
        assert!(copy.is_claimed());
        assert!(!copy.cancel());
        assert!(!copy.claim());
    }
}

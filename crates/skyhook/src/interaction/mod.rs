//! Approvals and questions go to the sole owner of the interface.
pub mod policy;

use crate::identity::AgentId;
use crate::{
    agent::{Question, QuestionError, QuestionFuture, QuestionHandler, QuestionReply},
    tool::policy::AuthorizationRequest,
};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{mpsc, oneshot};

pub enum PromptKind {
    Approval {
        request: AuthorizationRequest,
        reply: Reply<ApprovalReply>,
    },
    /// An agent's questions, each a choice among its suggestions or a written
    /// answer. A background batch cannot be dismissed, only answered or cancelled.
    Questions {
        questions: Vec<Question>,
        agent: AgentId,
        background: bool,
        reply: Reply<QuestionReply>,
    },
    /// The user renaming the session, starting from the title they set, if any.
    Rename {
        current: String,
        reply: Reply<String>,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalReply {
    Allow,
    Deny,
    Grant,
}

/// A category-specific, consuming reply capability.
pub type Reply<T> = oneshot::Sender<Result<T, Dismissed>>;

/// Why a prompt went unanswered, as whoever asked is told.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Dismissed {
    #[error("rename cancelled")]
    Rename,
    #[error("questions cancelled")]
    Questions,
    /// A new user prompt replaced the questions.
    #[error("questions cancelled by a new user prompt")]
    Superseded,
    /// The prompt was dropped without an answer.
    #[error("interaction cancelled")]
    Dropped,
    #[error("interface closed")]
    Closed,
}

pub struct Prompt {
    pub id: u64,
    pub kind: PromptKind,
}
impl Prompt {
    /// Shown at once, ahead of ordinary prompts: what the user just asked for
    /// themselves (a rename).
    pub fn preempts(&self) -> bool {
        matches!(&self.kind, PromptKind::Rename { .. })
    }

    pub fn is_closed(&self) -> bool {
        match &self.kind {
            PromptKind::Approval { reply, .. } => reply.is_closed(),
            PromptKind::Questions { reply, .. } => reply.is_closed(),
            PromptKind::Rename { reply, .. } => reply.is_closed(),
        }
    }

    /// Cancellation needs no answer payload, but still consumes the one-shot reply.
    pub fn reject(self, why: Dismissed) {
        match self.kind {
            PromptKind::Approval { reply, .. } => drop(reply.send(Err(why))),
            PromptKind::Questions { reply, .. } => drop(reply.send(Err(why))),
            PromptKind::Rename { reply, .. } => drop(reply.send(Err(why))),
        }
    }
}
#[derive(Clone)]
pub struct UiInteraction {
    tx: mpsc::UnboundedSender<Prompt>,
    counter: Arc<AtomicU64>,
}
impl UiInteraction {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<Prompt>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                tx,
                counter: Arc::new(AtomicU64::new(1)),
            },
            rx,
        )
    }
    /// A prompt and its answer, for whoever shows the prompt to deliver.
    fn pending<T, F: FnOnce(Reply<T>) -> PromptKind>(
        &self,
        pack: F,
    ) -> (
        Prompt,
        impl Future<Output = Result<T, Dismissed>> + use<T, F>,
    ) {
        let (reply, answer) = oneshot::channel();
        let prompt = Prompt {
            id: self.counter.fetch_add(1, Ordering::Relaxed),
            kind: pack(reply),
        };
        let answer = async move { answer.await.map_err(|_| Dismissed::Dropped)? };
        (prompt, answer)
    }
    pub(crate) async fn request<T>(
        &self,
        pack: impl FnOnce(Reply<T>) -> PromptKind,
    ) -> Result<T, Dismissed> {
        let (prompt, answer) = self.pending(pack);
        self.tx.send(prompt).map_err(|_| Dismissed::Closed)?;
        answer.await
    }

    /// A prompt for the session's new title, starting from `current`. The
    /// interface asking shows it itself, before handling any further input.
    pub fn rename(
        &self,
        current: String,
    ) -> (
        Prompt,
        impl Future<Output = Result<String, Dismissed>> + use<>,
    ) {
        self.pending(|reply| PromptKind::Rename { current, reply })
    }
}

impl QuestionHandler for UiInteraction {
    fn ask(&self, agent: AgentId, questions: Vec<Question>, background: bool) -> QuestionFuture {
        let this = self.clone();
        Box::pin(async move {
            this.request(|reply| PromptKind::Questions {
                questions,
                agent,
                background,
                reply,
            })
            .await
            .map_err(|why| QuestionError::Failed(why.to_string()))
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn interactions_report_every_cancellation() {
        let (handler, mut rx) = UiInteraction::new();
        let session = crate::identity::SessionId::from_bytes([1; 16]);
        let agent = crate::identity::AgentId::root(session);
        let ask = || {
            let question = Question {
                header: "header".parse().unwrap(),
                prompt: "prompt".parse().unwrap(),
                options: Vec::new(),
                multiple: false,
            };
            handler.ask(agent.clone(), vec![question], false)
        };
        let task = tokio::spawn(ask());
        rx.recv().await.unwrap().reject(Dismissed::Questions);
        let error = task.await.unwrap().unwrap_err().to_string();
        assert!(error.contains("questions cancelled"));
        let task = tokio::spawn(ask());
        drop(rx.recv().await.unwrap());
        let error = task.await.unwrap().unwrap_err().to_string();
        assert!(error.contains("interaction cancelled"));
        // An abandoned caller closes its prompt.
        let task = tokio::spawn(ask());
        let prompt = rx.recv().await.unwrap();
        task.abort();
        let _ = task.await;
        assert!(prompt.is_closed());
        drop(rx);
        let error = ask().await.unwrap_err().to_string();
        assert!(error.contains("interface closed"));
    }
}

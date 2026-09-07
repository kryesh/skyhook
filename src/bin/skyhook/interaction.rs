//! Host interactions go to the sole terminal owner, never independent stdin reads.
use serde_json::Value;
use skyhook::{
    agent::{Question, QuestionError, QuestionHandler},
    identity::AgentId,
    remote::{
        SecretValue, SensitivePrompt, SensitivePromptFuture, SensitivePromptHandler,
        SensitivePromptKind,
    },
    tool::policy::{
        AuthorizationRequest, Capability, PermissionUse, Policy, PolicyDecision, PolicyFuture,
    },
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::{mpsc, oneshot};

pub enum PromptKind {
    Approval(AuthorizationRequest),
    Questions {
        agent: AgentId,
        questions: Vec<Question>,
    },
    Authentication(SensitivePrompt),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalReply {
    Allow,
    Deny,
    Grant,
}

pub enum PromptResponse {
    Approval(ApprovalReply),
    Questions(Value),
    Authentication(SecretValue),
}

pub struct Prompt {
    pub id: u64,
    pub kind: PromptKind,
    pub reply: oneshot::Sender<Result<PromptResponse, String>>,
}
impl Prompt {
    pub fn secret(&self) -> bool {
        matches!(&self.kind, PromptKind::Authentication(prompt) if !matches!(prompt.kind, SensitivePromptKind::HostConfirmation | SensitivePromptKind::AgentConfirmation))
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
    async fn request(&self, kind: PromptKind) -> Result<PromptResponse, String> {
        let (reply, result) = oneshot::channel();
        self.tx
            .send(Prompt {
                id: self.counter.fetch_add(1, Ordering::Relaxed),
                kind,
                reply,
            })
            .map_err(|_| "interface closed".to_owned())?;
        result
            .await
            .map_err(|_| "interaction cancelled".to_owned())?
    }
}
fn requires_prompt(permission: &PermissionUse) -> bool {
    match permission.capability {
        Capability::Read | Capability::Agents => false,
        Capability::Exec | Capability::Targets => true,
        Capability::Write => {
            permission.resource.namespace != "workspace"
                || permission
                    .resource
                    .segments
                    .first()
                    .is_none_or(|target| target != "root")
        }
    }
}
impl Policy for UiInteraction {
    fn authorize(&self, request: AuthorizationRequest) -> PolicyFuture<'_> {
        Box::pin(async move {
            if !request.permissions.iter().any(requires_prompt) {
                return PolicyDecision::allow();
            }
            let grants = request
                .permissions
                .iter()
                .filter_map(|p| p.proposed_grant.clone())
                .collect();
            match self.request(PromptKind::Approval(request)).await {
                Ok(PromptResponse::Approval(ApprovalReply::Allow)) => PolicyDecision::allow(),
                Ok(PromptResponse::Approval(ApprovalReply::Grant)) => {
                    PolicyDecision::Allow { grants }
                }
                Ok(_) => PolicyDecision::Deny {
                    reason: "denied by user".into(),
                },
                Err(reason) => PolicyDecision::Deny { reason },
            }
        })
    }
}
impl QuestionHandler for UiInteraction {
    fn ask(
        &self,
        agent: AgentId,
        questions: Vec<Question>,
    ) -> Pin<Box<dyn Future<Output = Result<Value, QuestionError>> + Send + 'static>> {
        let this = self.clone();
        Box::pin(async move {
            match this
                .request(PromptKind::Questions { agent, questions })
                .await
            {
                Ok(PromptResponse::Questions(value)) => Ok(value),
                Ok(_) => Err(QuestionError::Failed("unexpected prompt response".into())),
                Err(error) => Err(QuestionError::Failed(error)),
            }
        })
    }
}
impl SensitivePromptHandler for UiInteraction {
    fn prompt(&self, prompt: SensitivePrompt) -> SensitivePromptFuture {
        let this = self.clone();
        Box::pin(async move {
            this.request(PromptKind::Authentication(prompt))
                .await
                .and_then(|response| match response {
                    PromptResponse::Authentication(secret) => Ok(secret),
                    _ => Err("unexpected prompt response".into()),
                })
                .map_err(skyhook::remote::SensitivePromptError::Failed)
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn abandoned_prompt_is_cancelled_without_reading_stdin() {
        let (handler, mut rx) = UiInteraction::new();
        let task = tokio::spawn(async move {
            handler
                .request(PromptKind::Authentication(SensitivePrompt {
                    kind: SensitivePromptKind::Password,
                    message: "password".into(),
                }))
                .await
        });
        let prompt = rx.recv().await.unwrap();
        task.abort();
        let _ = task.await;
        assert!(prompt.reply.is_closed());
    }
    fn password_prompt() -> SensitivePrompt {
        SensitivePrompt {
            kind: SensitivePromptKind::Password,
            message: "password".into(),
        }
    }

    #[tokio::test]
    async fn authentication_returns_secret_and_rejects_wrong_response_kind() {
        let (handler, mut rx) = UiInteraction::new();
        let task = tokio::spawn(handler.prompt(password_prompt()));
        let prompt = rx.recv().await.unwrap();
        assert!(
            prompt
                .reply
                .send(Ok(PromptResponse::Authentication(SecretValue::new(
                    "secret".into()
                ))))
                .is_ok()
        );
        assert_eq!(task.await.unwrap().unwrap().expose(), "secret");

        let task = tokio::spawn(handler.prompt(password_prompt()));
        let prompt = rx.recv().await.unwrap();
        assert!(
            prompt
                .reply
                .send(Ok(PromptResponse::Questions(Value::String(
                    "not a secret reply".into()
                ))))
                .is_ok()
        );
        assert!(task.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn authentication_preserves_explicit_and_dropped_reply_cancellation() {
        let (handler, mut rx) = UiInteraction::new();
        let task = tokio::spawn(handler.prompt(password_prompt()));
        let prompt = rx.recv().await.unwrap();
        assert!(
            prompt
                .reply
                .send(Err("authentication cancelled".into()))
                .is_ok()
        );
        assert!(
            task.await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("authentication cancelled")
        );

        let task = tokio::spawn(handler.prompt(password_prompt()));
        drop(rx.recv().await.unwrap());
        assert!(
            task.await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("interaction cancelled")
        );
    }
}

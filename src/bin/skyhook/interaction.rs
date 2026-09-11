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
        AuthorizationRequest, Capability, CapabilitySet, PermissionUse, Policy, PolicyDecision,
        PolicyFuture,
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
        // These capabilities are enforced by the core capability gate, not an
        // approval dialog. MCP adapters normally declare requires only.
        Capability::Read | Capability::Agents | Capability::Interactive | Capability::Mcp => false,
        Capability::Exec | Capability::Targets | Capability::Network => true,
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
/// Host approval defaults, usable without a terminal or prompt channel.
///
/// Capability checks remain the core's responsibility. This policy additionally
/// requires Interactive before asking for approval; it never turns a missing
/// capability into an approval dialog. --approve-all selects AllowAll instead
/// of this policy and does not enable questions or authentication.
pub struct HostApprovalPolicy {
    capabilities: CapabilitySet,
    ui: Option<UiInteraction>,
}

impl HostApprovalPolicy {
    pub fn new(capabilities: CapabilitySet, ui: Option<UiInteraction>) -> Self {
        Self { capabilities, ui }
    }

    fn decision_without_prompt(&self, permissions: &[PermissionUse]) -> Option<PolicyDecision> {
        if !permissions.iter().any(requires_prompt) {
            return Some(PolicyDecision::allow());
        }
        if !self.capabilities.contains(Capability::Interactive) {
            return Some(PolicyDecision::Deny {
                reason: "approval requires the interactive capability".into(),
            });
        }
        if self.ui.is_none() {
            return Some(PolicyDecision::Deny {
                reason: "approval requires an interactive interface".into(),
            });
        }
        None
    }
}

impl Policy for HostApprovalPolicy {
    fn authorize(&self, request: AuthorizationRequest) -> PolicyFuture<'_> {
        Box::pin(async move {
            if let Some(decision) = self.decision_without_prompt(&request.permissions) {
                return decision;
            }
            let grants = request
                .permissions
                .iter()
                .filter_map(|p| p.proposed_grant.clone())
                .collect();
            let ui = self.ui.as_ref().expect("approval interface checked above");
            match ui.request(PromptKind::Approval(request)).await {
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
// Preserve the existing UI-only policy API for embedders and test fixtures.
// Launch uses HostApprovalPolicy with the actual configured capabilities.
impl Policy for UiInteraction {
    fn authorize(&self, request: AuthorizationRequest) -> PolicyFuture<'_> {
        Box::pin(async move {
            HostApprovalPolicy::new(CapabilitySet::default(), Some(self.clone()))
                .authorize(request)
                .await
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
    use skyhook::tool::policy::ResourceId;

    fn permission(capability: Capability, namespace: &str, target: &str) -> PermissionUse {
        PermissionUse::new(capability, ResourceId::new(namespace, [target, "item"]))
    }

    fn capabilities(interactive: bool) -> CapabilitySet {
        let mut capabilities = CapabilitySet::default();
        if interactive {
            capabilities.insert(Capability::Interactive);
        } else {
            capabilities.remove(Capability::Interactive);
        }
        capabilities
    }

    #[test]
    fn automatic_approvals_need_neither_interactive_nor_a_ui() {
        let permissions = [
            permission(Capability::Read, "other", "build"),
            permission(Capability::Agents, "other", "build"),
            permission(Capability::Write, "workspace", "root"),
            // Gating-only capabilities must not become human approvals.
            permission(Capability::Interactive, "other", "root"),
            permission(Capability::Mcp, "other", "root"),
        ];
        for interactive in [false, true] {
            let policy = HostApprovalPolicy::new(capabilities(interactive), None);
            assert_eq!(
                policy.decision_without_prompt(&[]),
                Some(PolicyDecision::allow())
            );
            assert_eq!(
                policy.decision_without_prompt(&permissions),
                Some(PolicyDecision::allow()),
            );
        }
    }

    #[test]
    fn approval_required_operations_deny_without_interactive_or_ui() {
        let approvals = [
            permission(Capability::Exec, "workspace", "root"),
            permission(Capability::Targets, "other", "root"),
            permission(Capability::Network, "network", "root"),
            permission(Capability::Network, "network", "build"),
            PermissionUse::new(
                Capability::Write,
                ResourceId::new("workspace", Vec::<String>::new()),
            ),
            permission(Capability::Write, "workspace", "build"),
            permission(Capability::Write, "other", "root"),
        ];
        for interactive in [false, true] {
            let policy = HostApprovalPolicy::new(capabilities(interactive), None);
            for approval in &approvals {
                // An auto-allowed permission cannot mask another permission's
                // need for approval.
                let permissions = [
                    permission(Capability::Read, "workspace", "root"),
                    approval.clone(),
                ];
                let Some(PolicyDecision::Deny { reason }) =
                    policy.decision_without_prompt(&permissions)
                else {
                    panic!("approval-required request was not denied");
                };
                assert!(reason.contains("interactive"));
            }
        }
    }

    #[test]
    fn missing_interactive_never_queues_approval_even_with_live_ui() {
        let (ui, mut rx) = UiInteraction::new();
        let policy = HostApprovalPolicy::new(capabilities(false), Some(ui));
        assert!(matches!(
            policy.decision_without_prompt(&[permission(Capability::Exec, "workspace", "root")]),
            Some(PolicyDecision::Deny { .. }),
        ));
        assert_eq!(
            policy.decision_without_prompt(&[permission(Capability::Write, "workspace", "root")]),
            Some(PolicyDecision::allow()),
        );
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn interactive_ui_preserves_approval_prompts() {
        let (ui, _rx) = UiInteraction::new();
        let policy = HostApprovalPolicy::new(capabilities(true), Some(ui));
        assert_eq!(
            policy.decision_without_prompt(&[permission(Capability::Exec, "workspace", "root")]),
            None,
        );
    }

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

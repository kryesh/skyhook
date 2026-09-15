//! Host interactions go to the sole terminal owner, never independent stdin reads.
use serde_json::Value;
use skyhook::{
    agent::{Question, QuestionError, QuestionHandler},
    identity::AgentId,
    remote::{
        SecretValue, SensitivePrompt, SensitivePromptFuture, SensitivePromptHandler,
        SensitivePromptKind,
    },
    target::ROOT_TARGET,
    tool::policy::{
        AuthorizationRequest, Capability, CapabilitySet, PermissionUse, Policy, PolicyDecision,
        PolicyFuture, ResourceId,
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
    Approval {
        request: AuthorizationRequest,
        reply: Reply<ApprovalReply>,
    },
    Questions {
        agent: AgentId,
        questions: Vec<Question>,
        background: bool,
        reply: Reply<Value>,
    },
    Authentication {
        prompt: SensitivePrompt,
        reply: Reply<SecretValue>,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalReply {
    Allow,
    Deny,
    Grant,
}

/// A category-specific, consuming reply capability. Not cloneable:
/// authentication answers must remain `SecretValue` throughout this bridge.
pub type Reply<T> = oneshot::Sender<Result<T, String>>;

pub struct Prompt {
    pub id: u64,
    pub kind: PromptKind,
}
impl Prompt {
    pub fn secret(&self) -> bool {
        matches!(&self.kind, PromptKind::Authentication { prompt, .. } if !matches!(prompt.kind, SensitivePromptKind::HostConfirmation | SensitivePromptKind::AgentConfirmation))
    }

    pub fn is_closed(&self) -> bool {
        match &self.kind {
            PromptKind::Approval { reply, .. } => reply.is_closed(),
            PromptKind::Questions { reply, .. } => reply.is_closed(),
            PromptKind::Authentication { reply, .. } => reply.is_closed(),
        }
    }

    /// Cancellation needs no answer payload, but still consumes the one-shot reply.
    pub fn reject(self, error: String) {
        match self.kind {
            PromptKind::Approval { reply, .. } => drop(reply.send(Err(error))),
            PromptKind::Questions { reply, .. } => drop(reply.send(Err(error))),
            PromptKind::Authentication { reply, .. } => drop(reply.send(Err(error))),
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
    async fn request<T>(&self, pack: impl FnOnce(Reply<T>) -> PromptKind) -> Result<T, String> {
        let (reply, result) = oneshot::channel();
        self.tx
            .send(Prompt {
                id: self.counter.fetch_add(1, Ordering::Relaxed),
                kind: pack(reply),
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
        Capability::Write => !matches!(
            &permission.resource,
            ResourceId::Workspace { target, .. } if target == ROOT_TARGET
        ),
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
            match ui
                .request(|reply| PromptKind::Approval { request, reply })
                .await
            {
                Ok(ApprovalReply::Allow) => PolicyDecision::allow(),
                Ok(ApprovalReply::Grant) => PolicyDecision::Allow { grants },
                Ok(ApprovalReply::Deny) => PolicyDecision::Deny {
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
        self.ask_with_background(agent, questions, false)
    }
    fn ask_with_background(
        &self,
        agent: AgentId,
        questions: Vec<Question>,
        background: bool,
    ) -> Pin<Box<dyn Future<Output = Result<Value, QuestionError>> + Send + 'static>> {
        let this = self.clone();
        Box::pin(async move {
            this.request(|reply| PromptKind::Questions {
                agent,
                questions,
                background,
                reply,
            })
            .await
            .map_err(QuestionError::Failed)
        })
    }
}
impl SensitivePromptHandler for UiInteraction {
    fn prompt(&self, prompt: SensitivePrompt) -> SensitivePromptFuture {
        let this = self.clone();
        Box::pin(async move {
            this.request(|reply| PromptKind::Authentication { prompt, reply })
                .await
                .map_err(skyhook::remote::SensitivePromptError::Failed)
        })
    }
}
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use Capability::*;

    // AuthorizationRequest contains core-private provenance. Capture a real
    // core-produced request rather than introducing a test-only public ctor.
    pub(crate) async fn approval_request() -> AuthorizationRequest {
        let root = tempfile::tempdir().unwrap();
        let config: skyhook::config::Config = toml::from_str("[providers.test]\nkind='openai'\napi='chat_completions'\nbase_url='http://127.0.0.1:1'\n[models.first]\nprovider='test'\nmodel='fixture'\nmax_context=128000\nmax_output=4096\n").unwrap();
        let (ui, mut rx) = UiInteraction::new();
        let harness = config
            .into_runtime()
            .unwrap()
            .select_model("first")
            .unwrap()
            .harness_builder(root.path())
            .unwrap()
            .session_root(root.path().join("sessions"))
            .policy(Arc::new(ui))
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let operation = tokio::spawn({
            let session = session.clone();
            async move {
                let script = "return await tool.exec({argv: ['true']});";
                session.run_script(script).await
            }
        });
        let prompt = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv());
        let prompt = prompt.await.unwrap().unwrap();
        let PromptKind::Approval { request, .. } = &prompt.kind else {
            panic!("expected approval");
        };
        let request = request.clone();
        // The subprocess is never approved or executed by this fixture.
        prompt.reject("fixture captures only".into());
        let _ = operation.await.unwrap();
        session.shutdown().await.unwrap();
        request
    }

    fn workspace(target: &str) -> ResourceId {
        ResourceId::workspace(target, std::path::Path::new("item"))
    }

    fn custom(target: &str) -> ResourceId {
        ResourceId::custom("other", [target, "item"]).unwrap()
    }

    fn policy(interactive: bool, ui: Option<UiInteraction>) -> HostApprovalPolicy {
        let mut capabilities = CapabilitySet::default();
        if interactive {
            capabilities.insert(Capability::Interactive);
        } else {
            capabilities.remove(Capability::Interactive);
        }
        HostApprovalPolicy::new(capabilities, ui)
    }

    fn password_prompt() -> SensitivePrompt {
        SensitivePrompt {
            kind: SensitivePromptKind::Password,
            message: "password".into(),
        }
    }

    #[test]
    fn automatic_approvals_need_no_ui_and_approval_required_operations_deny_without_one() {
        let automatic = [
            PermissionUse::new(Read, custom("build")),
            PermissionUse::new(Agents, custom("build")),
            PermissionUse::new(Write, workspace("root")),
            // Gating-only capabilities must not become human approvals.
            PermissionUse::new(Interactive, custom("root")),
            PermissionUse::new(Mcp, custom("root")),
        ];
        let approvals = [
            PermissionUse::new(Exec, workspace("root")),
            PermissionUse::new(Targets, custom("root")),
            PermissionUse::new(Network, ResourceId::network("root", "https://example.com")),
            PermissionUse::new(Network, ResourceId::network("build", "https://example.com")),
            PermissionUse::new(Write, workspace("build")),
            PermissionUse::new(Write, custom("root")),
        ];
        for interactive in [false, true] {
            let policy = policy(interactive, None);
            assert_eq!(
                policy.decision_without_prompt(&[]),
                Some(PolicyDecision::allow())
            );
            let decision = policy.decision_without_prompt(&automatic);
            assert_eq!(decision, Some(PolicyDecision::allow()));
            for approval in &approvals {
                // An auto-allowed permission cannot mask another's need for approval.
                let permissions = [
                    PermissionUse::new(Read, workspace("root")),
                    approval.clone(),
                ];
                let decision = policy.decision_without_prompt(&permissions);
                assert!(
                    matches!(decision, Some(PolicyDecision::Deny { reason }) if reason.contains("interactive"))
                );
            }
        }
    }

    #[test]
    fn a_live_ui_is_prompted_only_with_interactive() {
        let exec = [PermissionUse::new(Exec, workspace("root"))];
        let (ui, mut rx) = UiInteraction::new();
        let policy = policy(false, Some(ui));
        let decision = policy.decision_without_prompt(&exec);
        assert!(matches!(decision, Some(PolicyDecision::Deny { .. })));
        let write = [PermissionUse::new(Write, workspace("root"))];
        assert_eq!(
            policy.decision_without_prompt(&write),
            Some(PolicyDecision::allow())
        );
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        let (ui, _rx) = UiInteraction::new();
        assert_eq!(
            super::tests::policy(true, Some(ui)).decision_without_prompt(&exec),
            None
        );
    }

    #[tokio::test]
    async fn authentication_returns_secrets_and_reports_every_cancellation_without_stdin() {
        let (handler, mut rx) = UiInteraction::new();
        let task = tokio::spawn(handler.prompt(password_prompt()));
        let prompt = rx.recv().await.unwrap();
        assert!(prompt.secret());
        let PromptKind::Authentication { reply, .. } = prompt.kind else {
            panic!("expected authentication request");
        };
        // This reply only accepts SecretValue, so no cross-category answer exists.
        assert!(reply.send(Ok(SecretValue::new("secret".into()))).is_ok());
        assert_eq!(task.await.unwrap().unwrap().expose(), "secret");

        let task = tokio::spawn(handler.prompt(password_prompt()));
        rx.recv()
            .await
            .unwrap()
            .reject("authentication cancelled".into());
        let error = task.await.unwrap().unwrap_err().to_string();
        assert!(error.contains("authentication cancelled"));
        let task = tokio::spawn(handler.prompt(password_prompt()));
        drop(rx.recv().await.unwrap());
        let error = task.await.unwrap().unwrap_err().to_string();
        assert!(error.contains("interaction cancelled"));
        // An abandoned caller closes its prompt.
        let task = tokio::spawn(handler.prompt(password_prompt()));
        let prompt = rx.recv().await.unwrap();
        task.abort();
        let _ = task.await;
        assert!(prompt.is_closed());
        drop(rx);
        let error = handler
            .prompt(password_prompt())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("interface closed"));
    }

    #[tokio::test]
    async fn approval_replies_preserve_decisions_and_only_proposed_grants() {
        use skyhook::tool::policy::ApprovalGrant;
        let mut request = approval_request().await;
        let grant = ApprovalGrant::exact(Exec, workspace("root"));
        request.permissions =
            vec![PermissionUse::new(Exec, workspace("root")).with_grant(grant.clone())];
        let deny = |reason: &str| PolicyDecision::Deny {
            reason: reason.into(),
        };
        for (answer, expected) in [
            (Ok(ApprovalReply::Allow), PolicyDecision::allow()),
            (Ok(ApprovalReply::Deny), deny("denied by user")),
            (
                Ok(ApprovalReply::Grant),
                PolicyDecision::Allow {
                    grants: vec![grant.clone()],
                },
            ),
            (
                Err("explicit cancellation".into()),
                deny("explicit cancellation"),
            ),
        ] {
            let (ui, mut rx) = UiInteraction::new();
            let request = request.clone();
            let task = tokio::spawn(async move { ui.authorize(request).await });
            let PromptKind::Approval { reply, .. } = rx.recv().await.unwrap().kind else {
                panic!("approval");
            };
            assert!(reply.send(answer).is_ok());
            assert_eq!(task.await.unwrap(), expected);
        }
    }
}

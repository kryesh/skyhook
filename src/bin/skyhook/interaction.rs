//! Host interactions go to the sole terminal owner, never independent stdin reads.
use serde_json::Value;
use skyhook::{
    agent::{Question, QuestionError, QuestionHandler},
    identity::AgentId,
    remote::{PromptAnswer, SensitivePrompt, SensitivePromptFuture, SensitivePromptHandler},
    target::TargetRef,
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
        reply: Reply<PromptAnswer>,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalReply {
    Allow,
    Deny,
    Grant,
}

/// A category-specific, consuming reply capability. Not cloneable:
/// authentication answers stay typed `PromptAnswer`s throughout this bridge.
pub type Reply<T> = oneshot::Sender<Result<T, String>>;

pub struct Prompt {
    pub id: u64,
    pub kind: PromptKind,
}
impl Prompt {
    pub fn secret(&self) -> bool {
        matches!(&self.kind, PromptKind::Authentication { prompt, .. } if !prompt.kind.is_confirmation())
    }

    /// Whether typed text is part of the answer: a question's answer or comment,
    /// or a secret. Approvals and confirmations are answered by choice alone.
    pub fn takes_text(&self) -> bool {
        matches!(&self.kind, PromptKind::Questions { .. }) || self.secret()
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
        Capability::Exec | Capability::Targets | Capability::SshAgent | Capability::Network => true,
        Capability::Write => !matches!(
            &permission.resource,
            ResourceId::Workspace { target, .. } if target == TargetRef::Root.as_str()
        ),
    }
}
/// Host approval defaults, usable without a terminal or prompt channel.
///
/// Capability checks remain the core's responsibility. This policy additionally
/// requires Interactive before asking for approval; it never turns a missing
/// capability into an approval dialog. --approve-all selects AllowAll instead
/// of this policy and does not enable questions or authentication.
pub enum HostApprovalPolicy {
    /// No interface: operations needing approval are denied.
    Unattended,
    /// Asks `ui` while the session holds the interactive capability.
    Attended {
        capabilities: CapabilitySet,
        ui: UiInteraction,
    },
}

/// Settled by the policy itself, or handed to the interface.
enum Approval<'a> {
    Decided(PolicyDecision),
    Ask(&'a UiInteraction),
}

impl HostApprovalPolicy {
    fn approval(&self, permissions: &[PermissionUse]) -> Approval<'_> {
        if !permissions.iter().any(requires_prompt) {
            return Approval::Decided(PolicyDecision::allow());
        }
        match self {
            Self::Unattended => Approval::Decided(PolicyDecision::Deny {
                reason: "approval requires an interactive interface".into(),
            }),
            Self::Attended { capabilities, .. }
                if !capabilities.contains(Capability::Interactive) =>
            {
                Approval::Decided(PolicyDecision::Deny {
                    reason: "approval requires the interactive capability".into(),
                })
            }
            Self::Attended { ui, .. } => Approval::Ask(ui),
        }
    }
}

impl Policy for HostApprovalPolicy {
    fn authorize(&self, request: AuthorizationRequest) -> PolicyFuture<'_> {
        Box::pin(async move {
            let ui = match self.approval(&request.permissions) {
                Approval::Decided(decision) => return decision,
                Approval::Ask(ui) => ui,
            };
            let grants = request
                .permissions
                .iter()
                .filter_map(PermissionUse::proposed_grant)
                .collect();
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

impl QuestionHandler for UiInteraction {
    fn ask(
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

    pub(crate) fn test_config() -> skyhook::config::Config {
        skyhook::config::Config::from_yaml("providers:\n  test:\n    dialect: compatible\n    codec: chat_completions\n    base_url: http://127.0.0.1:1\n    models:\n      first:\n        model: fixture\n        max_context: 128000\n        max_output: 4096\n").unwrap()
    }

    // AuthorizationRequest contains core-private provenance. Capture a real
    // core-produced request rather than introducing a test-only public ctor.
    pub(crate) async fn approval_request() -> AuthorizationRequest {
        let root = tempfile::tempdir().unwrap();
        let config = test_config();
        let (ui, mut rx) = UiInteraction::new();
        let harness = config
            .into_runtime()
            .unwrap()
            .select_model(&"test/first".parse().unwrap())
            .unwrap()
            .harness_builder(root.path())
            .unwrap()
            .session_root(root.path().join("sessions"))
            .policy(Arc::new(attended(true, ui)))
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let operation = tokio::spawn({
            let session = session.clone();
            async move {
                let script = "return await tool.exec({command:['true']});";
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
        let item = skyhook::tool::policy::PathText::new("item").unwrap();
        ResourceId::workspace(&target.parse().unwrap(), &item)
    }

    fn mcp_item(target: &str) -> ResourceId {
        ResourceId::mcp(target, "item")
    }

    fn attended(interactive: bool, ui: UiInteraction) -> HostApprovalPolicy {
        let mut capabilities = CapabilitySet::default();
        if interactive {
            capabilities.insert(Capability::Interactive);
        } else {
            capabilities.remove(Capability::Interactive);
        }
        HostApprovalPolicy::Attended { capabilities, ui }
    }

    /// What the policy settles by itself; `None` when it would ask the interface.
    fn decided(
        policy: &HostApprovalPolicy,
        permissions: &[PermissionUse],
    ) -> Option<PolicyDecision> {
        match policy.approval(permissions) {
            Approval::Decided(decision) => Some(decision),
            Approval::Ask(_) => None,
        }
    }

    fn password_prompt() -> SensitivePrompt {
        SensitivePrompt {
            kind: skyhook::remote::SensitivePromptKind::Password,
            message: "password".into(),
        }
    }

    #[test]
    fn automatic_approvals_need_no_ui_and_approval_required_operations_deny_without_one() {
        let automatic = [
            PermissionUse::new(Read, mcp_item("build")),
            PermissionUse::new(Agents, mcp_item("build")),
            PermissionUse::new(Write, workspace("root")),
            // Gating-only capabilities must not become human approvals.
            PermissionUse::new(Interactive, mcp_item("root")),
            PermissionUse::new(Mcp, mcp_item("root")),
        ];
        let approvals = [
            PermissionUse::new(Exec, workspace("root")),
            PermissionUse::new(Targets, mcp_item("root")),
            PermissionUse::new(
                Network,
                ResourceId::network(&skyhook::target::TargetRef::Root, "https://example.com"),
            ),
            PermissionUse::new(
                Network,
                ResourceId::network(&"build".parse().unwrap(), "https://example.com"),
            ),
            PermissionUse::new(Write, workspace("build")),
            PermissionUse::new(Write, mcp_item("root")),
        ];
        let (ui, _rx) = UiInteraction::new();
        for policy in [HostApprovalPolicy::Unattended, attended(false, ui)] {
            assert_eq!(decided(&policy, &[]), Some(PolicyDecision::allow()));
            assert_eq!(decided(&policy, &automatic), Some(PolicyDecision::allow()));
            for approval in &approvals {
                // An auto-allowed permission cannot mask another's need for approval.
                let permissions = [
                    PermissionUse::new(Read, workspace("root")),
                    approval.clone(),
                ];
                let decision = decided(&policy, &permissions);
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
        let policy = attended(false, ui);
        assert!(matches!(
            decided(&policy, &exec),
            Some(PolicyDecision::Deny { .. })
        ));
        let write = [PermissionUse::new(Write, workspace("root"))];
        assert_eq!(decided(&policy, &write), Some(PolicyDecision::allow()));
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        let (ui, _rx) = UiInteraction::new();
        assert_eq!(decided(&attended(true, ui), &exec), None);
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
        // This reply only accepts PromptAnswer, so no cross-category answer exists.
        let secret = skyhook::remote::SecretValue::new("secret".into());
        assert!(reply.send(Ok(PromptAnswer::Secret(secret))).is_ok());
        let PromptAnswer::Secret(answer) = task.await.unwrap().unwrap() else {
            panic!("expected a secret answer")
        };
        assert_eq!(answer.expose(), "secret");

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
        request.permissions = vec![PermissionUse::exact(Exec, workspace("root"))];
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
            let policy = attended(true, ui);
            let task = tokio::spawn(async move { policy.authorize(request).await });
            let PromptKind::Approval { reply, .. } = rx.recv().await.unwrap().kind else {
                panic!("approval");
            };
            assert!(reply.send(answer).is_ok());
            assert_eq!(task.await.unwrap(), expected);
        }
    }
}

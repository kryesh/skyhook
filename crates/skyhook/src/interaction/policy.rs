//! The host's approval policy: what needs a person to approve, and who is asked.
use super::{ApprovalReply, PromptKind, UiInteraction};
use crate::tool::policy::{
    Approval, AuthorizationRequest, Capability, PermissionUse, Policy, PolicyDecision, ResourceId,
};

fn requires_prompt(permission: &PermissionUse) -> bool {
    match permission.capability {
        // These capabilities are enforced by the core capability gate, not an
        // approval dialog. MCP adapters normally declare requires only.
        Capability::Read | Capability::Agents | Capability::Interactive | Capability::Mcp => false,
        Capability::Exec | Capability::Targets | Capability::Network | Capability::Host(_) => true,
        // A workspace is written unasked on the session host and on a target alike:
        // a target's own connection was approved before anything reached it.
        Capability::Write => !matches!(&permission.resource, ResourceId::Workspace { .. }),
    }
}
/// Host approval defaults, usable without a terminal or prompt channel.
pub enum HostApprovalPolicy {
    /// No interface: operations needing approval are denied.
    Unattended,
    /// Asks `ui`, the interface that makes the session interactive.
    Attended(UiInteraction),
}

impl Policy for HostApprovalPolicy {
    fn authorize(&self, request: AuthorizationRequest) -> Approval<'_> {
        if !request.permissions.iter().any(requires_prompt) {
            return Approval::Decided(PolicyDecision::allow());
        }
        let ui = match self {
            Self::Unattended => {
                return Approval::Decided(PolicyDecision::Deny {
                    reason: "approval requires an interactive interface".into(),
                });
            }
            Self::Attended(ui) => ui,
        };
        Approval::Ask(Box::pin(async move {
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
                Ok(ApprovalReply::Deny) => PolicyDecision::refused(),
                Err(why) => PolicyDecision::Deny {
                    reason: why.to_string(),
                },
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interaction::Dismissed;
    use Capability::*;
    use std::assert_matches;

    /// A root agent's request to run `true` in the workspace.
    fn approval_request() -> AuthorizationRequest {
        let session = crate::identity::SessionId::from_bytes([1; 16]);
        AuthorizationRequest {
            agent: crate::identity::AgentId::root(session),
            job: skyhook_worker::job::JobId::new(1).unwrap(),
            parent: None,
            tool: "exec".into(),
            permissions: vec![PermissionUse::exact(Exec, workspace("root"))],
            arguments: serde_json::json!({"tool": {"command": ["true"]}}),
        }
    }

    /// An item of `target`'s workspace.
    fn workspace(target: &str) -> ResourceId {
        ResourceId::workspace(&target.parse().unwrap(), &"item".into())
    }

    fn mcp_item(target: &str) -> ResourceId {
        ResourceId::mcp(target, "item")
    }

    /// What the policy settles by itself; `None` when it would ask the interface.
    fn decided(
        policy: &HostApprovalPolicy,
        permissions: &[PermissionUse],
    ) -> Option<PolicyDecision> {
        let request = AuthorizationRequest {
            permissions: permissions.to_vec(),
            ..approval_request()
        };
        match policy.authorize(request) {
            Approval::Decided(decision) => Some(decision),
            Approval::Ask(_) => None,
        }
    }

    #[test]
    fn automatic_approvals_need_no_ui_and_others_ask_one_or_deny_without() {
        let automatic = [
            PermissionUse::new(Read, mcp_item("build")),
            PermissionUse::new(Agents, mcp_item("build")),
            PermissionUse::new(Write, workspace("root")),
            PermissionUse::new(Write, workspace("build")),
            // Gating-only capabilities must not become human approvals.
            PermissionUse::new(Interactive, mcp_item("root")),
            PermissionUse::new(Mcp, mcp_item("root")),
        ];
        let approvals = [
            PermissionUse::new(Exec, workspace("root")),
            PermissionUse::new(Targets, mcp_item("root")),
            PermissionUse::new(
                Network,
                ResourceId::network(
                    &skyhook_tools::target::TargetRef::Root,
                    "https://example.com",
                ),
            ),
            PermissionUse::new(
                Network,
                ResourceId::network(&"build".parse().unwrap(), "https://example.com"),
            ),
            PermissionUse::new(Write, mcp_item("root")),
        ];
        let policy = HostApprovalPolicy::Unattended;
        assert_eq!(decided(&policy, &[]), Some(PolicyDecision::allow()));
        assert_eq!(decided(&policy, &automatic), Some(PolicyDecision::allow()));
        for approval in &approvals {
            // An auto-allowed permission cannot mask another's need for approval.
            let permissions = [
                PermissionUse::new(Read, workspace("root")),
                approval.clone(),
            ];
            let decision = decided(&policy, &permissions);
            assert_matches!(
                decision,
                Some(PolicyDecision::Deny { reason }) if reason.contains("interactive")
            );
        }
        let (ui, _rx) = UiInteraction::new();
        let exec = [PermissionUse::new(Exec, workspace("root"))];
        assert_eq!(decided(&HostApprovalPolicy::Attended(ui), &exec), None);
    }

    #[tokio::test]
    async fn approval_replies_preserve_decisions_and_only_proposed_grants() {
        use crate::tool::policy::ApprovalGrant;
        let mut request = approval_request();
        let grant = ApprovalGrant::exact(Exec, workspace("root"));
        request.permissions = vec![PermissionUse::exact(Exec, workspace("root"))];
        let deny = |reason: &str| PolicyDecision::Deny {
            reason: reason.into(),
        };
        for (answer, expected) in [
            (Ok(ApprovalReply::Allow), PolicyDecision::allow()),
            (Ok(ApprovalReply::Deny), PolicyDecision::refused()),
            (
                Ok(ApprovalReply::Grant),
                PolicyDecision::Allow {
                    grants: vec![grant.clone()],
                },
            ),
            (Err(Dismissed::Closed), deny("interface closed")),
        ] {
            let (ui, mut rx) = UiInteraction::new();
            let request = request.clone();
            let policy = HostApprovalPolicy::Attended(ui);
            let task = tokio::spawn(async move {
                let Approval::Ask(asked) = policy.authorize(request) else {
                    panic!("an interactive approval asks");
                };
                asked.await
            });
            let PromptKind::Approval { reply, .. } = rx.recv().await.unwrap().kind else {
                panic!("approval");
            };
            assert!(reply.send(answer).is_ok());
            assert_eq!(task.await.unwrap(), expected);
        }
    }
}

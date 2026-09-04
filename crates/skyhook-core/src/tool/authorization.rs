use std::{collections::HashMap, sync::Arc};

use futures_util::future::{BoxFuture, FutureExt as _, Shared};
use tokio::sync::Mutex;

use crate::{
    identity::{AgentId, JobId},
    job::CancellationToken,
};

use super::policy::{
    ApprovalGrant, AuthorizationRequest, CapabilitySet, PermissionUse, Policy, PolicyDecision,
};

#[derive(Clone, Debug)]
pub(crate) enum AuthorizationError {
    Denied(String),
    Cancelled,
    InvalidGrant(String),
    Unavailable,
}

#[derive(Clone)]
pub(crate) struct AuthorizationSubject {
    pub agent: AgentId,
    pub job: JobId,
    pub parent: Option<JobId>,
    pub scope: Option<u64>,
    pub capabilities: CapabilitySet,
    pub cancellation: CancellationToken,
}

type ApprovalFuture = Shared<BoxFuture<'static, Result<Vec<ApprovalGrant>, AuthorizationError>>>;

#[derive(Clone)]
struct PendingApproval {
    id: u64,
    future: ApprovalFuture,
}

#[derive(Clone)]
pub(crate) struct AuthorizationCoordinator {
    policy: Arc<dyn Policy>,
    state: Arc<Mutex<ApprovalState>>,
}

#[derive(Default)]
struct ApprovalState {
    grants: Vec<ApprovalGrant>,
    pending: HashMap<ApprovalGrant, PendingApproval>,
    next_pending: u64,
}

impl AuthorizationCoordinator {
    pub fn new(policy: Arc<dyn Policy>) -> Self {
        Self {
            policy,
            state: Arc::new(Mutex::new(ApprovalState::default())),
        }
    }

    pub async fn revoke(&self, predicate: impl Fn(&ApprovalGrant) -> bool) {
        let mut state = self.state.lock().await;
        state.grants.retain(|grant| !predicate(grant));
        state.pending.retain(|grant, _| !predicate(grant));
    }

    pub async fn authorize(
        &self,
        subject: &AuthorizationSubject,
        tool: String,
        mut permissions: Vec<PermissionUse>,
        arguments: serde_json::Value,
    ) -> Result<(), AuthorizationError> {
        if permissions
            .iter()
            .any(|use_| !subject.capabilities.contains(use_.capability))
        {
            return Err(AuthorizationError::Unavailable);
        }
        let mut unique = Vec::with_capacity(permissions.len());
        for permission in permissions {
            if !unique.contains(&permission) {
                unique.push(permission);
            }
        }
        permissions = unique;
        loop {
            let mut state = self.state.lock().await;
            permissions.retain(|use_| {
                !state
                    .grants
                    .iter()
                    .any(|grant| grant.covers(use_.capability, &use_.resource))
            });
            if permissions.is_empty() {
                return Ok(());
            }

            let proposals = permissions
                .iter()
                .filter_map(|use_| use_.proposed_grant.clone())
                .collect::<Vec<_>>();
            if let Some(pending) = proposals
                .iter()
                .find_map(|key| state.pending.get(key).cloned())
            {
                drop(state);
                self.wait(subject, pending.future).await?;
                continue;
            }

            state.next_pending += 1;
            let id = state.next_pending;
            let decision = self.decision(
                subject,
                tool.clone(),
                permissions.clone(),
                arguments.clone(),
                proposals.clone(),
            );
            let coordinator = self.clone();
            // Finalization belongs to the decision, not to any individual waiter.
            let task = tokio::spawn(async move {
                let result = tokio::spawn(decision).await.unwrap_or_else(|error| {
                    Err(AuthorizationError::Denied(format!(
                        "authorization policy failed: {error}"
                    )))
                });
                coordinator.finish_pending(id, &result).await;
                result
            });
            let future = async move {
                task.await.unwrap_or_else(|error| {
                    Err(AuthorizationError::Denied(format!(
                        "authorization task failed: {error}"
                    )))
                })
            }
            .boxed()
            .shared();
            for proposal in proposals {
                state.pending.insert(
                    proposal,
                    PendingApproval {
                        id,
                        future: future.clone(),
                    },
                );
            }
            drop(state);
            return self.wait(subject, future).await.map(|_| ());
        }
    }

    async fn finish_pending(
        &self,
        id: u64,
        result: &Result<Vec<ApprovalGrant>, AuthorizationError>,
    ) {
        let mut state = self.state.lock().await;
        if let Ok(grants) = result {
            for grant in grants {
                let current = state
                    .pending
                    .iter()
                    .any(|(proposal, pending)| pending.id == id && proposal.permits(grant));
                if current && !state.grants.contains(grant) {
                    state.grants.push(grant.clone());
                }
            }
        }
        state.pending.retain(|_, pending| pending.id != id);
    }

    fn decision(
        &self,
        subject: &AuthorizationSubject,
        tool: String,
        permissions: Vec<PermissionUse>,
        arguments: serde_json::Value,
        proposals: Vec<ApprovalGrant>,
    ) -> BoxFuture<'static, Result<Vec<ApprovalGrant>, AuthorizationError>> {
        let policy = self.policy.clone();
        let request = AuthorizationRequest {
            agent: subject.agent.clone(),
            job: subject.job,
            parent: subject.parent,
            scope: subject.scope,
            tool,
            permissions,
            arguments,
        };
        async move {
            match policy.authorize(request).await {
                PolicyDecision::Deny { reason } => Err(AuthorizationError::Denied(reason)),
                PolicyDecision::Allow { grants } => {
                    if let Some(grant) = grants
                        .iter()
                        .find(|grant| !proposals.iter().any(|proposal| proposal.permits(grant)))
                    {
                        return Err(AuthorizationError::InvalidGrant(format!(
                            "policy returned an unproposed grant for {:?}",
                            grant.capability
                        )));
                    }
                    Ok(grants)
                }
            }
        }
        .boxed()
    }

    async fn wait(
        &self,
        subject: &AuthorizationSubject,
        approval: ApprovalFuture,
    ) -> Result<Vec<ApprovalGrant>, AuthorizationError> {
        tokio::select! {
            result = approval => result,
            () = subject.cancellation.cancelled() => Err(AuthorizationError::Cancelled),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::{Notify, Semaphore};

    use super::*;
    use crate::{
        identity::SessionId,
        tool::policy::{ApprovalCoverage, Capability, PolicyFuture, ResourceId},
    };

    struct GrantingPolicy {
        calls: AtomicUsize,
    }

    impl Policy for GrantingPolicy {
        fn authorize(&self, request: AuthorizationRequest) -> PolicyFuture<'_> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let grants = request
                .permissions
                .into_iter()
                .filter_map(|permission| permission.proposed_grant)
                .collect();
            Box::pin(async move { PolicyDecision::Allow { grants } })
        }
    }

    struct BlockingPolicy {
        calls: AtomicUsize,
        started: Notify,
        release: Arc<Semaphore>,
    }

    impl Policy for BlockingPolicy {
        fn authorize(&self, request: AuthorizationRequest) -> PolicyFuture<'_> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_waiters();
            let release = self.release.clone();
            let grants = request
                .permissions
                .into_iter()
                .filter_map(|permission| permission.proposed_grant)
                .collect();
            Box::pin(async move {
                release.acquire().await.unwrap().forget();
                PolicyDecision::Allow { grants }
            })
        }
    }

    fn subject(cancellation: CancellationToken) -> AuthorizationSubject {
        AuthorizationSubject {
            agent: AgentId::root(SessionId::from_bytes([9; 16])),
            job: JobId::new(1).unwrap(),
            parent: None,
            scope: None,
            capabilities: CapabilitySet::default(),
            cancellation,
        }
    }

    fn permission(resource: ResourceId) -> PermissionUse {
        PermissionUse::new(Capability::Write, resource.clone())
            .with_grant(ApprovalGrant::exact(Capability::Write, resource))
    }

    #[tokio::test]
    async fn grants_are_cached_for_arbitrary_resources() {
        let policy = Arc::new(GrantingPolicy {
            calls: AtomicUsize::new(0),
        });
        let coordinator = AuthorizationCoordinator::new(policy.clone());
        let subject = subject(CancellationToken::new());
        let resource = ResourceId::new("plugin", ["server", "operation"]);
        for _ in 0..2 {
            coordinator
                .authorize(
                    &subject,
                    "dynamic".to_owned(),
                    vec![permission(resource.clone())],
                    serde_json::Value::Null,
                )
                .await
                .unwrap();
        }
        assert_eq!(policy.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancelling_one_waiter_does_not_poison_shared_approval() {
        let policy = Arc::new(BlockingPolicy {
            calls: AtomicUsize::new(0),
            started: Notify::new(),
            release: Arc::new(Semaphore::new(0)),
        });
        let coordinator = AuthorizationCoordinator::new(policy.clone());
        let first_cancellation = CancellationToken::new();
        let first_subject = subject(first_cancellation.clone());
        let second_subject = subject(CancellationToken::new());
        let resource = ResourceId::new("anything", ["shared"]);
        let first = tokio::spawn({
            let coordinator = coordinator.clone();
            let permission = permission(resource.clone());
            async move {
                coordinator
                    .authorize(
                        &first_subject,
                        "first".to_owned(),
                        vec![permission],
                        serde_json::Value::Null,
                    )
                    .await
            }
        });
        while policy.calls.load(Ordering::SeqCst) == 0 {
            policy.started.notified().await;
        }
        let second = tokio::spawn({
            let coordinator = coordinator.clone();
            let permission = permission(resource);
            async move {
                coordinator
                    .authorize(
                        &second_subject,
                        "second".to_owned(),
                        vec![permission],
                        serde_json::Value::Null,
                    )
                    .await
            }
        });
        tokio::task::yield_now().await;
        first_cancellation.cancel();
        assert!(matches!(
            first.await.unwrap(),
            Err(AuthorizationError::Cancelled)
        ));
        policy.release.add_permits(1);
        second.await.unwrap().unwrap();
        assert_eq!(policy.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn exact_proposals_cannot_be_widened() {
        let resource = ResourceId::new("opaque", ["one"]);
        let proposed = ApprovalGrant::exact(Capability::Write, resource.clone());
        let widened = ApprovalGrant {
            capability: Capability::Write,
            resource,
            coverage: ApprovalCoverage::Descendants,
        };
        assert!(!proposed.permits(&widened));
    }
}

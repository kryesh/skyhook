use std::{collections::HashMap, sync::Arc};

use futures_util::future::{BoxFuture, FutureExt as _, Shared};
use tokio::sync::Mutex;

use crate::{
    identity::{AgentId, JobId},
    job::CancellationToken,
};

use super::{
    AdmissionError,
    policy::{
        ApprovalGrant, AuthorizationRequest, CapabilitySet, PermissionUse, Policy, PolicyDecision,
    },
};
use crate::session::RecordSeq;

#[derive(Clone, Debug, thiserror::Error)]
pub(crate) enum AuthorizationError {
    #[error("{0}")]
    Denied(String),
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    InvalidGrant(String),
    /// The named tool needs a capability its subject lacks.
    #[error("tool `{0}` is unavailable in this context")]
    Unavailable(String),
    /// The policy stopped before deciding; nothing was denied or allowed.
    #[error("authorization could not be decided")]
    PolicyFailed,
}

/// The one tool-facing reading of an authorization failure, whichever path
/// (local, remote worker, or route) authorized. What the failure left behind is
/// the call site's to say: only admission knows nothing had started.
impl From<AuthorizationError> for AdmissionError {
    fn from(error: AuthorizationError) -> Self {
        match error {
            AuthorizationError::Denied(reason) => Self::denied(reason),
            AuthorizationError::Cancelled => Self::cancelled(),
            AuthorizationError::Unavailable(tool) => Self::unavailable(&tool),
            AuthorizationError::InvalidGrant(_) | AuthorizationError::PolicyFailed => {
                Self::failed(error)
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct AuthorizationSubject {
    pub agent: AgentId,
    pub job: JobId,
    pub parent: Option<JobId>,
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
    /// Session grants are journaled so a resumed session keeps them.
    journal: Option<crate::session::SessionStore>,
}

#[derive(Default)]
struct ApprovalState {
    /// Each grant with the sequence that journaled it, if any.
    grants: Vec<(Option<RecordSeq>, ApprovalGrant)>,
    pending: HashMap<ApprovalGrant, PendingApproval>,
    next_pending: u64,
}

impl AuthorizationCoordinator {
    pub fn new(policy: Arc<dyn Policy>) -> Self {
        Self {
            policy,
            state: Arc::new(Mutex::new(ApprovalState::default())),
            journal: None,
        }
    }

    /// Journal grants to `store`, starting from those it already holds.
    pub async fn journaled(mut self, store: crate::session::SessionStore) -> Self {
        let mut grants = Vec::new();
        for record in store.records().await {
            match record.event {
                crate::session::SessionEvent::ApprovalGranted { grant, .. } => {
                    grants.push((Some(record.sequence), grant));
                }
                crate::session::SessionEvent::ApprovalRevoked { grant } => {
                    grants.retain(|(sequence, _)| *sequence != Some(grant));
                }
                _ => {}
            }
        }
        self.state.lock().await.grants = grants;
        self.journal = Some(store);
        self
    }

    pub async fn revoke(&self, predicate: impl Fn(&ApprovalGrant) -> bool) {
        let mut state = self.state.lock().await;
        let mut revoked = Vec::new();
        state.grants.retain(|(sequence, grant)| {
            let keep = !predicate(grant);
            if !keep && let Some(sequence) = sequence {
                revoked.push(*sequence);
            }
            keep
        });
        state.pending.retain(|grant, _| !predicate(grant));
        if let Some(store) = &self.journal
            && !revoked.is_empty()
        {
            let root = crate::identity::AgentId::root(store.id());
            let events = revoked
                .into_iter()
                .map(|grant| {
                    (
                        root.clone(),
                        crate::session::SessionEvent::ApprovalRevoked { grant },
                    )
                })
                .collect();
            // Revocation already applies in memory. A lost record restores the grant
            // on resume, but route grants name target revisions, so a redefined
            // target still needs approval.
            let _ = store.append_all(events).await;
        }
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
            return Err(AuthorizationError::Unavailable(tool));
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
                    .any(|(_, grant)| grant.covers(use_.capability, &use_.resource))
            });
            if permissions.is_empty() {
                return Ok(());
            }

            let proposals = permissions
                .iter()
                .filter_map(PermissionUse::proposed_grant)
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
            let agent = subject.agent.clone();
            // Finalization belongs to the decision, not to any individual waiter.
            let task = tokio::spawn(async move {
                let result = tokio::spawn(decision)
                    .await
                    .unwrap_or(Err(AuthorizationError::PolicyFailed));
                coordinator.finish_pending(id, agent, &result).await;
                result
            });
            let future = async move { task.await.unwrap_or(Err(AuthorizationError::PolicyFailed)) }
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
        agent: AgentId,
        result: &Result<Vec<ApprovalGrant>, AuthorizationError>,
    ) {
        let mut state = self.state.lock().await;
        let mut accepted = Vec::new();
        if let Ok(grants) = result {
            for grant in grants {
                let current = state
                    .pending
                    .iter()
                    .any(|(proposal, pending)| pending.id == id && proposal.permits(grant));
                let known = state.grants.iter().any(|(_, known)| known == grant);
                if current && !known && !accepted.contains(grant) {
                    accepted.push(grant.clone());
                }
            }
        }
        let sequences = match &self.journal {
            Some(store) if !accepted.is_empty() => {
                let events = accepted
                    .iter()
                    .map(|grant| {
                        let grant = grant.clone();
                        let event = crate::session::SessionEvent::ApprovalGranted { grant };
                        (agent.clone(), event)
                    })
                    .collect();
                match store.append_all(events).await {
                    Ok(records) => records.iter().map(|record| Some(record.sequence)).collect(),
                    // An unjournaled grant is not kept; later requests ask again.
                    Err(_) => Vec::new(),
                }
            }
            _ => accepted.iter().map(|_| None).collect(),
        };
        state.grants.extend(sequences.into_iter().zip(accepted));
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

    /// Grants every proposal, optionally waiting for a release permit first.
    #[derive(Default)]
    struct CountingPolicy {
        calls: AtomicUsize,
        started: Notify,
        release: Option<Arc<Semaphore>>,
    }

    impl Policy for CountingPolicy {
        fn authorize(&self, request: AuthorizationRequest) -> PolicyFuture<'_> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_waiters();
            let release = self.release.clone();
            let grants = request
                .permissions
                .into_iter()
                .filter_map(|p| p.proposed_grant())
                .collect();
            Box::pin(async move {
                if let Some(release) = release {
                    release.acquire().await.unwrap().forget();
                }
                PolicyDecision::Allow { grants }
            })
        }
    }

    fn subject(cancellation: CancellationToken) -> AuthorizationSubject {
        AuthorizationSubject {
            agent: AgentId::root(SessionId::from_bytes([9; 16])),
            job: JobId::new(1).unwrap(),
            parent: None,
            capabilities: CapabilitySet::default(),
            cancellation,
        }
    }

    async fn authorize(
        coordinator: &AuthorizationCoordinator,
        subject: &AuthorizationSubject,
        resource: &ResourceId,
    ) -> Result<(), AuthorizationError> {
        let permission = PermissionUse::exact(Capability::Write, resource.clone());
        let arguments = serde_json::Value::Null;
        coordinator
            .authorize(subject, "tool".to_owned(), vec![permission], arguments)
            .await
            .map(drop)
    }

    #[tokio::test]
    async fn journaled_grants_survive_a_resumed_coordinator_until_revoked() {
        let session = crate::session::fixture::MemorySession::new().await;
        let jobs = crate::job::JobManager::new(session.store.clone());
        let spec = crate::job::JobSpec::test(session.agent.clone(), "tool");
        let job = jobs.create(spec).await.unwrap();
        let mut subject = subject(CancellationToken::new());
        (subject.agent, subject.job) = (session.agent.clone(), job.id());
        let resource = ResourceId::mcp("server", "operation");
        let policy = Arc::new(CountingPolicy::default());
        let journaled = async || {
            AuthorizationCoordinator::new(policy.clone())
                .journaled(session.store.clone())
                .await
        };
        authorize(&journaled().await, &subject, &resource)
            .await
            .unwrap();
        // A resumed session's coordinator starts from the journal, not the policy.
        let resumed = journaled().await;
        authorize(&resumed, &subject, &resource).await.unwrap();
        assert_eq!(policy.calls.load(Ordering::SeqCst), 1);
        resumed.revoke(|_| true).await;
        authorize(&journaled().await, &subject, &resource)
            .await
            .unwrap();
        assert_eq!(policy.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn grants_are_cached_for_arbitrary_resources() {
        let policy = Arc::new(CountingPolicy::default());
        let coordinator = AuthorizationCoordinator::new(policy.clone());
        let subject = subject(CancellationToken::new());
        let resource = ResourceId::mcp("server", "operation");
        for _ in 0..2 {
            authorize(&coordinator, &subject, &resource).await.unwrap();
        }
        assert_eq!(policy.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancelling_one_waiter_does_not_poison_shared_approval() {
        let release = Arc::new(Semaphore::new(0));
        let policy = Arc::new(CountingPolicy {
            release: Some(release.clone()),
            ..Default::default()
        });
        let coordinator = AuthorizationCoordinator::new(policy.clone());
        let first_cancellation = CancellationToken::new();
        let resource = ResourceId::session("shared");
        let spawn = |subject: AuthorizationSubject| {
            let (coordinator, resource) = (coordinator.clone(), resource.clone());
            tokio::spawn(async move { authorize(&coordinator, &subject, &resource).await })
        };
        let first = spawn(subject(first_cancellation.clone()));
        while policy.calls.load(Ordering::SeqCst) == 0 {
            policy.started.notified().await;
        }
        let second = spawn(subject(CancellationToken::new()));
        tokio::task::yield_now().await;
        first_cancellation.cancel();
        assert!(matches!(
            first.await.unwrap(),
            Err(AuthorizationError::Cancelled)
        ));
        release.add_permits(1);
        second.await.unwrap().unwrap();
        assert_eq!(policy.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn exact_proposals_cannot_be_widened() {
        let resource = ResourceId::session("one");
        let proposed = ApprovalGrant::exact(Capability::Write, resource.clone());
        let widened = ApprovalGrant {
            capability: Capability::Write,
            resource,
            coverage: ApprovalCoverage::Descendants,
        };
        assert!(!proposed.permits(&widened));
    }
}

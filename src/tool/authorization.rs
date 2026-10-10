use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
};

use futures_util::future::{BoxFuture, FutureExt as _, Shared, WeakShared};

use crate::{
    identity::{AgentId, JobId},
    job::CancellationToken,
    session::SessionStore,
    target::{Revisions, TargetDefinition, TargetName, TargetRegistry, replaces},
};

use super::{
    AdmissionError,
    policy::{
        ApprovalGrant, AuthorizationRequest, CapabilitySet, PermissionUse, Policy, PolicyDecision,
        ResourceId,
    },
};

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
    /// The call names a target since redefined: a stale route or revision, or a
    /// pending decision the redefinition revoked. An approval never carries over
    /// to a replaced target.
    #[error("target was redefined")]
    TargetRedefined,
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
            AuthorizationError::InvalidGrant(_)
            | AuthorizationError::PolicyFailed
            | AuthorizationError::TargetRedefined => Self::failed(error),
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

/// What a policy sees of one invocation, for its admission and every later
/// request. The tool's own arguments keep their namespace.
#[derive(Debug, Default, serde::Serialize)]
pub(crate) struct AuthorizationArguments {
    pub(crate) tool: super::registry::Arguments,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) route: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<SourceArguments>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct SourceArguments {
    pub(crate) path: String,
    pub(crate) target: crate::target::TargetRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) route: Option<serde_json::Value>,
}

impl AuthorizationArguments {
    /// The document a request shows, naming the origin a running request moved to.
    pub(crate) fn document(&self, network_origin: Option<&str>) -> serde_json::Value {
        #[derive(serde::Serialize)]
        struct Document<'a> {
            #[serde(flatten)]
            arguments: &'a AuthorizationArguments,
            #[serde(skip_serializing_if = "Option::is_none")]
            network_origin: Option<&'a str>,
        }
        let document = Document {
            arguments: self,
            network_origin,
        };
        serde_json::to_value(document).expect("JSON values and strings serialize")
    }
}

/// What a running invocation asks its host to authorize.
#[derive(Debug)]
pub(crate) enum Reauthorization {
    /// Permissions derived where the invocation runs.
    Permissions(Vec<PermissionUse>),
    /// The origin a running request moved to, which the policy is also shown.
    Network(String),
}

impl Reauthorization {
    /// The permissions asked for on `target`, and the origin a request moved to.
    pub(crate) fn into_parts(
        self,
        target: &crate::target::TargetRef,
    ) -> (Vec<PermissionUse>, Option<String>) {
        match self {
            Self::Permissions(permissions) => (permissions, None),
            Self::Network(origin) => {
                let resource = super::policy::ResourceId::network(target, &origin);
                let permission = PermissionUse::new(super::policy::Capability::Network, resource);
                (vec![permission], Some(origin))
            }
        }
    }
}

/// A decision every call proposing one of its grants shares. Only the calls
/// waiting for it hold it: once the last stops waiting, the policy's prompt is
/// withdrawn.
type ApprovalFuture = Shared<BoxFuture<'static, Result<(), Ended>>>;

/// Why a shared decision did not allow what it was asked.
#[derive(Clone)]
enum Ended {
    Refused(AuthorizationError),
    /// A redefinition replaced a target it was asked about.
    Revoked,
}

#[derive(Clone)]
pub(crate) struct AuthorizationCoordinator {
    policy: Arc<dyn Policy>,
    state: Arc<Mutex<ApprovalState>>,
    /// The session whose target revisions grants and decisions count for; without
    /// it, every target counts as current.
    targets: Option<SessionStore>,
}

/// Grants last as long as the coordinator: a reopened session asks again.
#[derive(Default)]
struct ApprovalState {
    /// Each grant with the revisions of the targets its resource names, as approved.
    grants: Vec<(ApprovalGrant, Revisions)>,
    decisions: HashMap<u64, Decision>,
    next_decision: u64,
}

struct Decision {
    /// What it may grant: a call proposing any of these shares it.
    proposals: Vec<ApprovalGrant>,
    future: WeakShared<BoxFuture<'static, Result<(), Ended>>>,
    /// The revisions of the targets its call's permissions name, granted or not.
    revisions: Revisions,
    revoked: CancellationToken,
}

impl AuthorizationCoordinator {
    pub fn new(policy: Arc<dyn Policy>) -> Self {
        Self {
            policy,
            state: Arc::default(),
            targets: None,
        }
    }

    /// Count grants and decisions only for the revisions of `store`'s targets
    /// they were approved or asked for.
    pub(crate) fn with_targets(mut self, store: SessionStore) -> Self {
        self.targets = Some(store);
        self
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ApprovalState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Read the session's targets, if any are tracked.
    fn read<T>(&self, read: impl FnOnce(Option<&TargetRegistry>) -> T) -> T {
        match &self.targets {
            Some(store) => store.read(|state| read(Some(&state.targets))),
            None => read(None),
        }
    }

    /// Withdraw the decisions asked about a revision `redefined` replaces; a call
    /// waiting for one plans again. Grants need no revoking: each counts only
    /// for the revisions it was approved for.
    pub(crate) fn revoke(&self, redefined: &[TargetDefinition]) {
        let mut state = self.state();
        state.decisions.retain(|_, decision| {
            let revoked = replaces(redefined, &decision.revisions);
            if revoked {
                decision.revoked.cancel();
            }
            !revoked
        });
    }

    /// Ask the policy for what `permissions` need beyond the grants held, sharing
    /// a decision another call already awaits for the same proposal. A call
    /// naming a target revision since replaced, or asking from a connection
    /// `admitted` at one, is refused as redefined.
    pub async fn authorize(
        &self,
        subject: &AuthorizationSubject,
        tool: String,
        permissions: Vec<PermissionUse>,
        arguments: serde_json::Value,
        admitted: &[(TargetName, u64)],
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
        loop {
            let pending = self.pending(subject, &tool, &unique, &arguments, admitted)?;
            match pending {
                None => return Ok(()),
                Some(Pending::Asked(future)) => {
                    return self
                        .wait(subject, future)
                        .await
                        .map_err(|ended| match ended {
                            Ended::Revoked => AuthorizationError::TargetRedefined,
                            Ended::Refused(error) => error,
                        });
                }
                // Whatever another call's decision granted, or its revocation, is
                // checked again from the top.
                Some(Pending::Joined(future)) => {
                    if let Err(Ended::Refused(error)) = self.wait(subject, future).await {
                        return Err(error);
                    }
                }
            }
        }
    }

    /// The decision `permissions` wait for, asked by this call or joined from
    /// another; none once usable grants cover them. Target revisions are read under this
    /// lock, so no grant counts, and no decision is asked, for one replaced.
    fn pending(
        &self,
        subject: &AuthorizationSubject,
        tool: &str,
        permissions: &[PermissionUse],
        arguments: &serde_json::Value,
        admitted: &[(TargetName, u64)],
    ) -> Result<Option<Pending>, AuthorizationError> {
        let mut state = self.state();
        let grants = &mut state.grants;
        let asked = self.read(|targets| {
            grants.retain(|(_, approved)| current(targets, approved));
            let asked = (permissions.iter()).map(|use_| revisions(targets, &use_.resource));
            (current(targets, admitted).then(|| asked.collect::<Option<Vec<_>>>())).flatten()
        });
        let asked = asked.ok_or(AuthorizationError::TargetRedefined)?;
        let covered = |use_: &&PermissionUse| {
            (state.grants.iter()).any(|(grant, _)| grant.covers(use_.capability, &use_.resource))
        };
        let needed = (permissions.iter()).filter(|use_| !covered(use_)).cloned();
        let needed = needed.collect::<Vec<_>>();
        if needed.is_empty() {
            return Ok(None);
        }
        let proposals = (needed.iter())
            .filter_map(PermissionUse::proposed_grant)
            .collect::<Vec<_>>();
        state
            .decisions
            .retain(|_, decision| decision.future.upgrade().is_some());
        let shared = (state.decisions.values())
            .filter(|decision| decision.proposals.iter().any(|key| proposals.contains(key)))
            .find_map(|decision| decision.future.upgrade());
        if let Some(future) = shared {
            return Ok(Some(Pending::Joined(future)));
        }
        state.next_decision += 1;
        let id = state.next_decision;
        let revoked = CancellationToken::new();
        let decision = self.decision(
            subject,
            tool.to_owned(),
            needed,
            arguments.clone(),
            proposals.clone(),
        );
        let (coordinator, cancelled) = (self.clone(), revoked.clone());
        let future = async move {
            let result = tokio::select! {
                biased;
                () = cancelled.cancelled() => return Err(Ended::Revoked),
                result = decision => result,
            };
            coordinator.finish(id, result)
        };
        let future = future.boxed().shared();
        let decision = Decision {
            proposals,
            future: future.downgrade().expect("a new decision has not finished"),
            revisions: asked.into_iter().flatten().collect(),
            revoked,
        };
        state.decisions.insert(id, decision);
        Ok(Some(Pending::Asked(future)))
    }

    /// Keep what decision `id` granted, unless a redefinition replaced a target
    /// it was asked about: one that finishes after it is refused, as one before
    /// it is revoked.
    fn finish(
        &self,
        id: u64,
        result: Result<Vec<ApprovalGrant>, AuthorizationError>,
    ) -> Result<(), Ended> {
        let mut state = self.state();
        let decision = state.decisions.remove(&id).ok_or(Ended::Revoked)?;
        let grants = result.map_err(Ended::Refused)?;
        let approved = self.read(|targets| {
            let approved = grants.into_iter().filter_map(|grant| {
                let revisions = revisions(targets, &grant.resource)?;
                Some((grant, revisions))
            });
            current(targets, &decision.revisions).then(|| approved.collect::<Vec<_>>())
        });
        for (grant, revisions) in approved.ok_or(Ended::Revoked)? {
            state.grants.retain(|(known, _)| known != &grant);
            state.grants.push((grant, revisions));
        }
        Ok(())
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
    ) -> Result<(), Ended> {
        tokio::select! {
            result = approval => result,
            () = subject.cancellation.cancelled() => {
                Err(Ended::Refused(AuthorizationError::Cancelled))
            }
        }
    }
}

/// Whether every target in `revisions` is still at its revision, as grants,
/// decisions and the connections asking for them count only while they are.
fn current(targets: Option<&TargetRegistry>, revisions: &[(TargetName, u64)]) -> bool {
    targets.is_none_or(|targets| targets.current(revisions))
}

/// The revision of every target `resource` names, as `targets` now stand.
fn revisions(targets: Option<&TargetRegistry>, resource: &ResourceId) -> Option<Revisions> {
    targets.map_or(Some(Vec::new()), |targets| targets.revisions(resource))
}

/// A decision a call waits for.
enum Pending {
    Asked(ApprovalFuture),
    /// Another call's, for an overlapping proposal.
    Joined(ApprovalFuture),
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        identity::SessionId,
        tool::policy::{ApprovalCoverage, Capability, ResourceId},
    };

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
            .authorize(subject, "tool".to_owned(), vec![permission], arguments, &[])
            .await
            .map(drop)
    }

    #[tokio::test]
    async fn grants_are_cached_for_arbitrary_resources() {
        let (prompts, mut asked) = tokio::sync::mpsc::unbounded_channel();
        let policy = Arc::new(crate::tests::PromptingPolicy(prompts));
        let coordinator = AuthorizationCoordinator::new(policy);
        let subject = subject(CancellationToken::new());
        let resource = ResourceId::mcp("server", "operation");
        let first = authorize(&coordinator, &subject, &resource);
        let answer = async { asked.recv().await.unwrap().1.send(()).unwrap() };
        tokio::join!(first, answer).0.unwrap();
        authorize(&coordinator, &subject, &resource).await.unwrap();
        assert!(asked.try_recv().is_err());
    }

    /// A decision lasts while any call waits for it: cancelling one waiter
    /// leaves it to the others, whom it then allows, and once the last stops
    /// waiting the policy's prompt is withdrawn.
    #[tokio::test]
    async fn a_decision_lasts_while_any_call_waits_for_it() {
        for answered in [true, false] {
            let (prompts, mut asked) = tokio::sync::mpsc::unbounded_channel();
            let policy = Arc::new(crate::tests::PromptingPolicy(prompts));
            let coordinator = AuthorizationCoordinator::new(policy);
            let resource = ResourceId::session("shared");
            let first = CancellationToken::new();
            let spawned = tokio::spawn({
                let (coordinator, resource) = (coordinator.clone(), resource.clone());
                let subject = subject(first.clone());
                async move { authorize(&coordinator, &subject, &resource).await }
            });
            let (_, mut reply) = crate::tests::bounded(asked.recv()).await.unwrap();
            let second = CancellationToken::new();
            let second_subject = subject(second.clone());
            let mut waiting = Box::pin(authorize(&coordinator, &second_subject, &resource));
            assert!(futures_util::poll!(&mut waiting).is_pending());
            first.cancel();
            let cancelled = spawned.await.unwrap();
            assert!(matches!(cancelled, Err(AuthorizationError::Cancelled)));
            assert!(futures_util::poll!(&mut waiting).is_pending());
            assert!(!reply.is_closed());
            if answered {
                reply.send(()).unwrap();
                crate::tests::bounded(waiting).await.unwrap();
            } else {
                second.cancel();
                let cancelled = crate::tests::bounded(waiting).await;
                assert!(matches!(cancelled, Err(AuthorizationError::Cancelled)));
                crate::tests::bounded(reply.closed()).await;
            }
            assert!(asked.try_recv().is_err());
        }
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

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
};

use futures_util::future::{BoxFuture, FutureExt as _, Shared, WeakShared};

use crate::identity::AgentId;
use crate::target::{Revisions, TargetDefinition};
use crate::tool::policy::{ApprovalGrant, CapabilitySet, PermissionUse};
use crate::{
    job::CancellationToken,
    session::SessionStore,
    target::{TargetRegistry, replaces},
};
use skyhook_tools::arguments::Arguments;
use skyhook_tools::target::TargetName;
use skyhook_worker::job::JobId;

use super::policy::{Approval, AuthorizationRequest, Policy, PolicyDecision};

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
    pub(crate) tool: Arguments,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) route: Option<crate::target::RouteArguments>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<SourceArguments>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct SourceArguments {
    pub(crate) path: skyhook_worker::op::TargetPath,
    pub(crate) target: skyhook_tools::target::TargetRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) route: Option<crate::target::RouteArguments>,
}

impl AuthorizationArguments {
    /// The document a request shows.
    pub(crate) fn document(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("JSON values and strings serialize")
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
    /// The session whose target revisions grants and decisions count for.
    targets: SessionStore,
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
    /// Only a decision that may ask someone is shared: a `Never` refusal
    /// belongs to its own call.
    asking: Asking,
    future: WeakShared<BoxFuture<'static, Result<(), Ended>>>,
    /// The revisions of the targets its call's permissions name, granted or not.
    revisions: Revisions,
    revoked: CancellationToken,
}

impl AuthorizationCoordinator {
    /// A coordinator counting grants and decisions only for the revisions of
    /// `targets`' targets they were approved or asked for.
    pub fn new(policy: Arc<dyn Policy>, targets: SessionStore) -> Self {
        Self {
            policy,
            state: Arc::default(),
            targets,
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ApprovalState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Read the session's targets.
    fn read<T>(&self, read: impl FnOnce(&TargetRegistry) -> T) -> T {
        self.targets.read(|state| read(&state.targets))
    }

    /// Withdraw the decisions asked about a revision `redefined` replaces; a call
    /// waiting for one plans again. Grants need no revoking: each counts only
    /// for the revisions it was approved for.
    pub(crate) fn revoke(&self, redefined: &[TargetDefinition]) {
        let mut state = self.state();
        let revoked = |_: &u64, decision: &mut Decision| replaces(redefined, &decision.revisions);
        for (_, decision) in state.decisions.extract_if(revoked) {
            decision.revoked.cancel();
        }
    }

    /// Ask the policy for what `permissions` need beyond the grants held, sharing
    /// a decision another call already awaits for the same proposal. A call
    /// naming a target revision since replaced, or asking from a connection
    /// `admitted` at one, is refused as redefined.
    pub(crate) async fn authorize(
        &self,
        subject: &AuthorizationSubject,
        tool: String,
        permissions: Vec<PermissionUse>,
        arguments: serde_json::Value,
        admitted: &[(TargetName, u64)],
        asking: Asking,
    ) -> Result<(), AuthorizationError> {
        if permissions
            .iter()
            .any(|use_| !subject.capabilities.contains(&use_.capability))
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
            let pending = self.pending(subject, &tool, &unique, &arguments, admitted, asking)?;
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
    /// another; none once usable grants cover them, or they read the session's own
    /// spill files. Target revisions are read under this lock, so no grant counts,
    /// and no decision is asked, for one replaced.
    fn pending(
        &self,
        subject: &AuthorizationSubject,
        tool: &str,
        permissions: &[PermissionUse],
        arguments: &serde_json::Value,
        admitted: &[(TargetName, u64)],
        asking: Asking,
    ) -> Result<Option<Pending>, AuthorizationError> {
        let mut state = self.state();
        let grants = &mut state.grants;
        let asked = self.targets.read(|session| {
            let targets = &session.targets;
            grants.retain(|(_, approved)| targets.current(approved));
            let asked = (permissions.iter()).map(|use_| targets.revisions(&use_.resource));
            let asked = (targets.current(admitted))
                .then(|| asked.collect::<Option<Vec<_>>>())
                .flatten()?;
            let covered = |use_: &&PermissionUse| {
                session.spills.covers(use_, targets)
                    || (grants.iter())
                        .any(|(grant, _)| grant.covers(&use_.capability, &use_.resource))
            };
            let needed = (permissions.iter()).filter(|use_| !covered(use_)).cloned();
            Some((asked, needed.collect::<Vec<_>>()))
        });
        let (asked, needed) = asked.ok_or(AuthorizationError::TargetRedefined)?;
        if needed.is_empty() {
            return Ok(None);
        }
        let proposals = (needed.iter())
            .filter_map(PermissionUse::proposed_grant)
            .collect::<Vec<_>>();
        state
            .decisions
            .retain(|_, decision| decision.future.upgrade().is_some());
        // Only calls that may ask share a decision: one that may not must
        // neither refuse for one that may nor keep its prompt alive.
        let shared = (state.decisions.values())
            .filter(|decision| {
                (asking, decision.asking) == (Asking::Allowed, Asking::Allowed)
                    && decision.proposals.iter().any(|key| proposals.contains(key))
            })
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
            asking,
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
            asking,
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
                let revisions = targets.revisions(&grant.resource)?;
                Some((grant, revisions))
            });
            (targets.current(&decision.revisions)).then(|| approved.collect::<Vec<_>>())
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
        asking: Asking,
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
        let decided = async move {
            let decision = match (policy.authorize(request), asking) {
                (Approval::Decided(decision), _) => decision,
                (Approval::Ask(_), Asking::Never) => PolicyDecision::refused(),
                (Approval::Ask(asked), Asking::Allowed) => asked.await,
            };
            match decision {
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
        };
        // A policy that panics fails the decision for every call sharing it,
        // rather than unwinding the waiter that happens to poll it.
        let decided = std::panic::AssertUnwindSafe(decided).catch_unwind();
        decided
            .map(|decided| decided.unwrap_or(Err(AuthorizationError::PolicyFailed)))
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

/// A decision a call waits for.
enum Pending {
    Asked(ApprovalFuture),
    /// Another call's, for an overlapping proposal.
    Joined(ApprovalFuture),
}

/// Whether a decision may ask anyone. One for a call naming a path outside the
/// root that does not resolve may not: asking would tell the call it exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum Asking {
    Allowed,
    Never,
}

impl Asking {
    /// Whether a call whose path preflight left `unresolved` may ask anyone.
    pub const fn for_unresolved<T>(unresolved: &Option<T>) -> Self {
        match unresolved {
            Some(_) => Self::Never,
            None => Self::Allowed,
        }
    }
}

#[derive(Clone, Debug, thiserror::Error, serde::Deserialize, serde::Serialize)]
pub enum AuthorizationError {
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

/// The one tool-facing reading of an authorization failure. What the failure left
/// behind is the call site's to say: only admission knows nothing had started.
impl From<AuthorizationError> for skyhook_tools::error::AdmissionError {
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

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;
    use crate::identity::SessionId;
    use crate::tool::policy::{Capability, ResourceId};

    fn subject(cancellation: CancellationToken) -> AuthorizationSubject {
        AuthorizationSubject {
            agent: AgentId::root(SessionId::from_bytes([9; 16])),
            job: JobId::new(1).unwrap(),
            parent: None,
            capabilities: CapabilitySet::default(),
            cancellation,
        }
    }

    /// A coordinator over a new session's targets, and the session it keeps open.
    async fn coordinator(
        policy: Arc<dyn Policy>,
    ) -> (crate::tests::TestRuntime, AuthorizationCoordinator) {
        let session = crate::tests::TestRuntime::new().await;
        let coordinator = AuthorizationCoordinator::new(policy, session.store.clone());
        (session, coordinator)
    }

    async fn authorize(
        coordinator: &AuthorizationCoordinator,
        subject: &AuthorizationSubject,
        resource: &ResourceId,
    ) -> Result<(), AuthorizationError> {
        let permission = PermissionUse::exact(Capability::Write, resource.clone());
        let arguments = serde_json::Value::Null;
        coordinator
            .authorize(
                subject,
                "tool".to_owned(),
                vec![permission],
                arguments,
                &[],
                Asking::Allowed,
            )
            .await
            .map(drop)
    }

    /// A grant is cached for any resource; a denial is not, so the call asks again.
    #[tokio::test]
    async fn grants_are_cached_for_arbitrary_resources_but_denials_are_not() {
        let (prompts, mut asked) = tokio::sync::mpsc::unbounded_channel();
        let policy = Arc::new(crate::tests::PromptingPolicy(prompts));
        let (_session, coordinator) = coordinator(policy).await;
        let subject = subject(CancellationToken::new());
        let resource = ResourceId::mcp("server", "operation");
        let denied = authorize(&coordinator, &subject, &resource);
        let refuse = async { drop(asked.recv().await.unwrap()) };
        let denied = tokio::join!(denied, refuse).0;
        assert_matches!(denied, Err(AuthorizationError::Denied(_)));
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
            let (_session, coordinator) = coordinator(policy).await;
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
            assert_matches!(cancelled, Err(AuthorizationError::Cancelled));
            assert!(futures_util::poll!(&mut waiting).is_pending());
            assert!(!reply.is_closed());
            if answered {
                reply.send(()).unwrap();
                crate::tests::bounded(waiting).await.unwrap();
            } else {
                second.cancel();
                let cancelled = crate::tests::bounded(waiting).await;
                assert_matches!(cancelled, Err(AuthorizationError::Cancelled));
                crate::tests::bounded(reply.closed()).await;
            }
            assert!(asked.try_recv().is_err());
        }
    }

    /// Panics once released.
    struct PanickingPolicy(tokio::sync::mpsc::UnboundedSender<tokio::sync::oneshot::Sender<()>>);

    impl Policy for PanickingPolicy {
        fn authorize(&self, _: AuthorizationRequest) -> Approval<'_> {
            Approval::Ask(Box::pin(async move {
                let (release, released) = tokio::sync::oneshot::channel();
                self.0.send(release).unwrap();
                let _ = released.await;
                panic!("policy panicked")
            }))
        }
    }

    /// A policy that panics fails its one decision for every call sharing it.
    #[tokio::test]
    async fn a_panicking_policy_fails_every_call_waiting_on_it() {
        let (prompts, mut asked) = tokio::sync::mpsc::unbounded_channel();
        let (_session, coordinator) = coordinator(Arc::new(PanickingPolicy(prompts))).await;
        let (subject, resource) = (
            subject(CancellationToken::new()),
            ResourceId::session("one"),
        );
        let mut asking = Box::pin(authorize(&coordinator, &subject, &resource));
        let mut joined = Box::pin(authorize(&coordinator, &subject, &resource));
        assert!(futures_util::poll!(&mut asking).is_pending());
        assert!(futures_util::poll!(&mut joined).is_pending());
        crate::tests::bounded(asked.recv())
            .await
            .unwrap()
            .send(())
            .unwrap();
        let failed = crate::tests::bounded(futures_util::future::join(asking, joined)).await;
        assert_matches!(
            failed,
            (
                Err(AuthorizationError::PolicyFailed),
                Err(AuthorizationError::PolicyFailed)
            )
        );
        assert!(asked.try_recv().is_err());
    }

    /// Only calls that may ask share a decision: a call that may not ask
    /// anyone neither shares its refusal nor joins a prompt it cannot keep.
    #[tokio::test]
    async fn only_calls_that_may_ask_share_a_decision() {
        let (prompts, _asked) = tokio::sync::mpsc::unbounded_channel();
        let policy = Arc::new(crate::tests::PromptingPolicy(prompts));
        let (_session, coordinator) = coordinator(policy).await;
        let subject = subject(CancellationToken::new());
        let permissions = [PermissionUse::exact(
            Capability::Write,
            ResourceId::session("one"),
        )];
        let pending = |asking| {
            coordinator.pending(
                &subject,
                "tool",
                &permissions,
                &serde_json::Value::Null,
                &[],
                asking,
            )
        };
        let never = pending(Asking::Never);
        assert!(matches!(never, Ok(Some(Pending::Asked(_)))));
        let allowed = pending(Asking::Allowed);
        assert!(matches!(allowed, Ok(Some(Pending::Asked(_)))));
        assert!(matches!(
            pending(Asking::Never),
            Ok(Some(Pending::Asked(_)))
        ));
        drop((never, allowed));
    }
}

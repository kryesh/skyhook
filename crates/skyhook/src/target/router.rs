use crate::tool::authorization::AuthorizationError;
use crate::{
    remote::{PreparedConnection, RemoteError, RemoteManager},
    session::{Command, CoreState, Plan, SessionError, SessionEvent, SessionStore},
    tool::{
        ToolError,
        authorization::{AuthorizationCoordinator, AuthorizationSubject},
    },
};
use skyhook_tools::diagnostic::{FailureSite, PartialContext};
use skyhook_worker::diagnostic::{Effects, Operation, Subject};

use crate::target::TargetError;
use crate::target::{ConfiguredPasswords, PasswordUnconfigured, Route, TargetDefinition};
use skyhook_tools::target::TargetName;
use skyhook_worker::secret::Credential;

use super::TargetRegistry;

#[derive(Clone)]
pub(crate) struct TargetRouter {
    store: SessionStore,
    remote: RemoteManager,
    authorization: AuthorizationCoordinator,
    passwords: ConfiguredPasswords,
}

/// What a target's password reads as.
type Read = Result<Credential, PasswordUnconfigured>;

/// A registration's failure to commit: a refused or indeterminate append.
struct Unsaved(ToolError);

impl From<SessionError> for Unsaved {
    fn from(error: SessionError) -> Self {
        let effects = match &error {
            SessionError::JournalFailed(failure)
                if matches!(failure.cause(), SessionError::Unconfirmed(_)) =>
            {
                Effects::Unknown
            }
            _ => Effects::Unchanged,
        };
        Self(registration_error(error.into(), Operation::Save, effects))
    }
}

impl TargetRouter {
    pub fn new(
        store: SessionStore,
        remote: RemoteManager,
        authorization: AuthorizationCoordinator,
        passwords: ConfiguredPasswords,
    ) -> Self {
        Self {
            store,
            remote,
            authorization,
            passwords,
        }
    }

    /// The coordinator whose grants and decisions this router's redefinitions revoke.
    pub(crate) fn authorization(&self) -> &AuthorizationCoordinator {
        &self.authorization
    }

    pub(crate) async fn agent_environment(
        &self,
    ) -> Result<skyhook_worker::origin::ProcessEnvironment, RemoteError> {
        self.remote.agent_environment().await
    }

    pub(crate) async fn shutdown(&self) {
        self.remote.shutdown().await;
    }

    /// Add `definition` at its next revision. Its plan revokes the grants and
    /// decisions, and stops the pooled connections and their startups, that name
    /// a revision it replaces: the target's own or one its routes pass through.
    pub(crate) async fn add(
        &self,
        definition: TargetDefinition,
        subject: &AuthorizationSubject,
    ) -> Result<TargetDefinition, ToolError> {
        let rejected = |error: TargetError| {
            Unsaved(registration_error(
                error.into(),
                Operation::Validate,
                Effects::Unchanged,
            ))
        };
        let (agent, cancellation) = (subject.agent.clone(), subject.cancellation.clone());
        let router = self.clone();
        let upsert = move |state: &CoreState| {
            let destination = state.targets.upsert(definition).map_err(rejected)?;
            if cancellation.is_cancelled() {
                let cancelled = ToolError::cancelled();
                let error = registration_error(cancelled, Operation::Prepare, Effects::Unchanged);
                return Err(Unsaved(error));
            }
            let targets = vec![destination.clone()];
            let upserted = SessionEvent::TargetsUpserted {
                targets: targets.clone(),
            };
            Ok(Plan::new(vec![(agent, upserted)], move |state, _| {
                state.defer(move || {
                    router.authorization.revoke(&targets);
                    router.remote.invalidate(&targets);
                });
                destination
            }))
        };
        let committed = self.store.commit(Command::Journal, upsert).await;
        committed.map_err(|Unsaved(error)| error)
    }

    /// Each of the session's targets with the credential its password is, for one
    /// that reads a password.
    pub(crate) fn list(&self) -> Vec<(TargetDefinition, Option<Read>)> {
        let read = |target: TargetDefinition| {
            let password = target.connection.password();
            let read =
                password.map(|password| self.passwords.credential(&target.name, password).cloned());
            (target, read)
        };
        (self.targets(TargetRegistry::list).into_iter())
            .map(read)
            .collect()
    }

    /// The targets that read a password the configuration no longer gives them as the
    /// session holds them.
    pub(crate) fn unconfigured(&self) -> Vec<TargetName> {
        let unconfigured = |(target, password): (TargetDefinition, Option<Read>)| {
            matches!(password, Some(Err(_))).then_some(target.name)
        };
        self.list().into_iter().filter_map(unconfigured).collect()
    }

    /// Read the session's targets.
    pub(crate) fn targets<T>(&self, read: impl FnOnce(&TargetRegistry) -> T) -> T {
        self.store.read(|state| read(&state.targets))
    }

    /// The route to `target`.
    pub fn resolve(&self, target: &TargetName) -> Result<Route, TargetError> {
        self.targets(|targets| targets.route(target))
    }

    /// Connect along `route`, admitted only while every target on it is at the
    /// revision it was planned for, read against the session's targets as its
    /// pooled startup is joined or started, and once it connected. A replaced
    /// revision, or a startup a redefinition stopped, is refused as redefined:
    /// its call plans again.
    pub async fn prepare(
        &self,
        route: Route,
        workspace: Vec<skyhook_worker::op::TargetPath>,
        subject: &AuthorizationSubject,
    ) -> Result<PreparedConnection, RemoteError> {
        let redefined = || RemoteError::from(AuthorizationError::TargetRedefined);
        // A route that no longer resolves changed too; planning again reports why.
        let current = || {
            let current = self.resolve(&route.destination.name).ok();
            (current.is_some_and(|current| current.identity() == route.identity()))
                .then_some(())
                .ok_or_else(redefined)
        };
        let connecting =
            (self.remote).connection(route.clone(), workspace, &subject.cancellation, &current);
        let prepared = connecting.await?;
        match current() {
            // Admitted: later redefinition does not revoke this snapshot.
            Ok(()) if self.remote.is_current(&prepared) => Ok(prepared),
            refused => {
                self.remote.discard(&prepared);
                Err(refused.err().unwrap_or_else(redefined))
            }
        }
    }
}

/// Registration happens on the session host. Target validation keeps the argument
/// it rejected; other failures are attributed to the registration stage.
fn registration_error(error: ToolError, operation: Operation, effects: Effects) -> ToolError {
    error.or(
        PartialContext::new(operation, Subject::Label("target registration".into()))
            .at(FailureSite::Host)
            .effects(effects),
    )
}

#[cfg(test)]
impl TargetRouter {
    /// A router over `store`'s targets, approving with `policy`, whose connections
    /// use the default factory.
    pub(crate) fn over(
        store: &SessionStore,
        policy: std::sync::Arc<dyn crate::tool::policy::Policy>,
    ) -> Self {
        let remote = RemoteManager::new(Default::default(), Default::default(), Default::default());
        let authorization = AuthorizationCoordinator::new(policy, store.clone());
        Self::new(store.clone(), remote, authorization, Default::default())
    }

    /// [`over`](Self::over) `store`'s targets, with `definitions` added.
    pub(crate) async fn test(
        store: &SessionStore,
        definitions: impl IntoIterator<Item = TargetDefinition>,
        policy: std::sync::Arc<dyn crate::tool::policy::Policy>,
    ) -> Self {
        let targets = definitions.into_iter().collect();
        let agent = crate::identity::AgentId::root(store.id());
        let upserted = SessionEvent::TargetsUpserted { targets };
        store.append(agent, upserted).await.unwrap();
        Self::over(store, policy)
    }

    /// Connect with `factory` instead.
    pub(crate) fn with_connection_factory(
        mut self,
        factory: std::sync::Arc<dyn crate::remote::ConnectionFactory>,
    ) -> Self {
        self.remote = self.remote.with_connection_factory(factory);
        self
    }
}

#[cfg(test)]
mod tests {
    use crate::tool::policy::{Capability, PermissionUse, ResourceId};
    use std::assert_matches;
    use std::sync::{Arc, atomic::Ordering};

    use super::*;
    use crate::identity::{AgentId, SessionId};
    use crate::tool::authorization::Asking;
    use crate::{
        job::CancellationToken,
        remote::{ConnectionFactory, ShimFactory},
        tests::RecordingPolicy,
        tool::policy::PolicyDecision,
    };
    use skyhook_tools::diagnostic::{Cause, PartialDiagnostic};
    use skyhook_worker::job::JobId;

    /// Allows, adopting the proposed grants.
    fn recording() -> Arc<RecordingPolicy> {
        RecordingPolicy::deciding(|request| PolicyDecision::Allow {
            grants: (request.permissions.iter())
                .filter_map(PermissionUse::proposed_grant)
                .collect(),
        })
    }

    fn target(name: &str, via: Option<&str>) -> TargetDefinition {
        crate::target::tests::definition(name, format!("/{name}"), via)
    }

    fn name(name: &str) -> TargetName {
        name.parse().unwrap()
    }

    fn definitions(targets: &[(&str, Option<&str>)]) -> Vec<TargetDefinition> {
        targets
            .iter()
            .map(|(name, via)| target(name, *via))
            .collect()
    }

    #[test]
    fn routes_derive_permissions_and_arguments_from_their_definitions() {
        let edge = crate::target::tests::winrm("edge", None);
        let mut gateway = target("gateway", None);
        (gateway.revision, gateway.origin) = (7, Some(name("edge")));
        gateway.ssh_mut().user = Some("ops".parse().unwrap());
        let mut destination = target("build", Some("gateway"));
        (destination.revision, destination.origin) = (19, Some(name("edge")));
        let registry = super::TargetRegistry::from_definitions([edge, gateway, destination]);
        let route = registry.unwrap().route(&name("build")).unwrap();
        let hops = vec![(name("edge"), 1), (name("gateway"), 7), (name("build"), 19)];
        let resource = ResourceId::route(name("build"), hops);
        let permission = |capability| PermissionUse::exact(capability, resource.clone());
        assert_eq!(route.permissions(), [permission(Capability::Targets)]);
        // An approval shows where each hop leads and who logs in there, the origin
        // and the jump as well as the destination.
        let hop = |target: &str, user: Option<&str>| crate::target::RouteHop {
            name: name(target),
            host: format!("{target}.example.com"),
            user: user.map(str::to_owned),
        };
        assert_eq!(
            route.authorization_arguments().route,
            [
                hop("edge", Some("alice")),
                hop("gateway", Some("ops")),
                hop("build", None)
            ]
        );
    }

    fn subject() -> AuthorizationSubject {
        let mut capabilities = crate::tool::policy::CapabilitySet::default();
        capabilities.insert(Capability::Targets);
        AuthorizationSubject {
            agent: AgentId::root(SessionId::from_bytes([7; 16])),
            job: JobId::new(17).unwrap(),
            parent: Some(JobId::new(11).unwrap()),
            capabilities,
            cancellation: CancellationToken::new(),
        }
    }

    fn subject_for(store: &SessionStore) -> AuthorizationSubject {
        let mut subject = subject();
        subject.agent = AgentId::root(store.id());
        subject
    }

    async fn replace_target(router: &TargetRouter, definition: TargetDefinition) {
        let subject = subject_for(&router.store);
        router.add(definition, &subject).await.unwrap();
    }

    /// A router over a new session's targets, `targets` added.
    async fn router(
        targets: impl IntoIterator<Item = TargetDefinition>,
        policy: Arc<RecordingPolicy>,
        factory: Option<Arc<dyn ConnectionFactory>>,
    ) -> (tempfile::TempDir, TargetRouter) {
        let session = crate::tests::TestRuntime::new().await;
        let mut router = TargetRouter::test(&session.store, targets, policy).await;
        if let Some(factory) = factory {
            router = router.with_connection_factory(factory);
        }
        (session.root, router)
    }

    async fn prepare(
        router: &TargetRouter,
        subject: &AuthorizationSubject,
    ) -> Result<PreparedConnection, RemoteError> {
        let route = router.resolve(&name("build")).unwrap();
        approve(router, &route, subject).await?;
        router
            .prepare(route, vec!["/override".into()], subject)
            .await
    }

    async fn approve(
        router: &TargetRouter,
        route: &Route,
        subject: &AuthorizationSubject,
    ) -> Result<(), RemoteError> {
        let permissions = route.permissions();
        let arguments = serde_json::to_value(route.authorization_arguments())
            .expect("route arguments serialize");
        let authorize = router.authorization.authorize(
            subject,
            "connect_target".to_owned(),
            permissions,
            arguments,
            &[],
            Asking::Allowed,
        );
        Ok(authorize.await?)
    }

    fn spawn_prepare(
        router: &TargetRouter,
        subject: AuthorizationSubject,
    ) -> tokio::task::JoinHandle<Result<PreparedConnection, RemoteError>> {
        let router = router.clone();
        tokio::spawn(async move { prepare(&router, &subject).await })
    }

    #[tokio::test]
    async fn admitted_route_snapshot_survives_later_registry_mutation() {
        let factory = ShimFactory::new();
        let targets = definitions(&[("gateway", None), ("build", Some("gateway"))]);
        let (_root, router) = router(targets, recording(), Some(factory.clone())).await;
        let resolve = || (router.resolve(&name("build"))).unwrap();
        let admitted = resolve();
        factory.ready.add_permits(1);
        let prepared = prepare(&router, &subject()).await.unwrap();
        assert!(router.remote.is_current(&prepared));

        // Redefining a jump or the destination changes the route's identity.
        let mut previous = admitted.identity();
        for redefined in [target("gateway", None), target("build", Some("gateway"))] {
            replace_target(&router, redefined).await;
            let current = resolve().identity();
            assert_ne!(previous, current);
            previous = current;
        }
        assert_eq!(admitted.destination.revision, 1);
        // Invalidation removes pool reuse; it does not consume the admitted handle.
        assert!(!router.remote.is_current(&prepared));
        assert_eq!(factory.starts.load(Ordering::SeqCst), 1);
        drop(prepared);
        router.shutdown().await;
    }

    /// A route redefined before it connects is refused as redefined without
    /// starting or asking anything, and one redefined while its connection, or
    /// its origin's, starts stops that startup and drops what it started: its
    /// caller plans again.
    #[tokio::test]
    async fn a_stale_route_snapshot_is_refused_and_its_startup_stopped() {
        for (during_startup, origin) in [(false, false), (true, false), (true, true)] {
            let policy = recording();
            let factory = ShimFactory::new();
            let mut build = target("build", Some("gateway"));
            if origin {
                (build.via, build.origin) = (None, Some(name("gateway")));
            }
            let targets = [target("gateway", None), build];
            let (_root, router) = router(targets, policy.clone(), Some(factory.clone())).await;
            let prepared = if during_startup {
                let preparing = spawn_prepare(&router, subject());
                factory.wait_for_hello().await;
                replace_target(&router, target("gateway", None)).await;
                crate::tests::bounded(preparing).await.unwrap()
            } else {
                let subject = subject();
                let stale = (router.resolve(&name("build"))).unwrap();
                approve(&router, &stale, &subject).await.unwrap();
                replace_target(&router, target("gateway", None)).await;
                crate::tests::bounded(router.prepare(stale, vec!["/override".into()], &subject))
                    .await
            };
            assert!(matches!(
                prepared.err().map(|refused| refused.fault),
                Some(crate::remote::RemoteFault::Authorization(
                    AuthorizationError::TargetRedefined
                ))
            ));
            let started = usize::from(during_startup);
            crate::tests::bounded(async {
                while factory.counts() != (started, started) {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            assert_eq!(policy.requests.lock().unwrap().len(), 1);
            router.shutdown().await;
        }
    }

    #[tokio::test]
    async fn cancelled_registration_changes_nothing() {
        let (_root, router) = router([], recording(), None).await;
        let subject = subject_for(&router.store);
        subject.cancellation.cancel();
        let error = router
            .add(target("cancelled", None), &subject)
            .await
            .unwrap_err();
        let diagnostic = error.diagnostic();
        assert_eq!(diagnostic.cause, Cause::Cancelled);
        assert_eq!(diagnostic.context.site, FailureSite::Host);
        assert_eq!(diagnostic.context.effects, Effects::Unchanged);
        let added = router.targets(|targets| targets.get(&name("cancelled")));
        assert!(added.is_err());
        router.shutdown().await;
    }

    /// A registration that fails to save reports what it may have changed: an
    /// append the journal definitely refused changed nothing; one whose journal
    /// failed once it committed may have saved it.
    #[tokio::test]
    async fn a_failed_target_save_reports_what_it_may_have_changed() {
        use crate::session::{SessionError, tests::journal_failure};
        for (cause, effects) in [
            (SessionError::Unconfirmed(None), Effects::Unknown),
            (SessionError::WrongSession, Effects::Unchanged),
        ] {
            let Unsaved(error) = SessionError::JournalFailed(journal_failure(cause)).into();
            let diagnostic = error.diagnostic();
            assert_eq!(diagnostic.context.operation, Operation::Save);
            assert_eq!(diagnostic.context.effects, effects);
        }
        // A mismatched session rejects append after normalization and graph validation.
        let (_root, router) = router(definitions(&[("first", None)]), recording(), None).await;
        let error = router
            .add(target("second", None), &subject())
            .await
            .unwrap_err();
        let diagnostic = error.diagnostic();
        assert_eq!(
            diagnostic.cause,
            PartialDiagnostic::from(&SessionError::WrongSession).cause
        );
        assert_eq!(diagnostic.context.operation, Operation::Save);
        assert_eq!(diagnostic.context.site, FailureSite::Host);
        assert_eq!(diagnostic.context.effects, Effects::Unchanged);
        assert!(
            router
                .targets(|targets| targets.get(&name("second")))
                .is_err()
        );
    }

    /// A grant counts only for the target revisions it was approved for: once a
    /// redefinition commits, no grant naming that target, or one routed through
    /// it, is usable, whatever its resource, even before its revoke runs. A call
    /// naming a replaced route, or a request from a connection admitted at one,
    /// is refused unasked, even where a grant at the new revision covers it.
    #[tokio::test]
    async fn a_redefinition_leaves_no_grant_naming_its_target_usable() {
        let policy = recording();
        let targets = [target("build", None), target("deep", Some("build"))];
        let (_root, router) = router(targets, policy.clone(), None).await;
        let mut caller = subject_for(&router.store);
        caller.capabilities.insert(Capability::Write);
        let path = |target: &str| ResourceId::path(&target.parse().unwrap(), "/src");
        let mcp = ResourceId::mcp("server", "operation");
        let asked = || policy.requests.lock().unwrap().len();
        let authorize = async |resource: &ResourceId| {
            let permission = PermissionUse::exact(Capability::Write, resource.clone());
            let (tool, arguments) = ("tool".to_owned(), serde_json::Value::Null);
            let authorize = (router.authorization).authorize(
                &caller,
                tool,
                vec![permission],
                arguments,
                &[],
                Asking::Allowed,
            );
            authorize.await.map(|()| asked())
        };
        for resource in [path("build"), path("deep"), mcp.clone()] {
            authorize(&resource).await.unwrap();
        }
        assert_eq!(asked(), 3);
        let stale = (router.resolve(&name("build"))).unwrap();
        replace_target(&router, target("build", None)).await;
        assert_eq!(authorize(&mcp).await.unwrap(), 3);
        assert_eq!(authorize(&path("build")).await.unwrap(), 4);
        assert_eq!(authorize(&path("deep")).await.unwrap(), 5);
        let (tool, arguments) = ("connect_target".to_owned(), serde_json::Value::Null);
        let route = (router.authorization).authorize(
            &caller,
            tool,
            stale.permissions(),
            arguments,
            &[],
            Asking::Allowed,
        );
        assert_matches!(route.await, Err(AuthorizationError::TargetRedefined));
        let (tool, arguments) = ("tool".to_owned(), serde_json::Value::Null);
        let permission = PermissionUse::exact(Capability::Write, path("build"));
        let admitted = stale.identity();
        let forwarded = (router.authorization).authorize(
            &caller,
            tool,
            vec![permission],
            arguments,
            &admitted,
            Asking::Allowed,
        );
        assert_matches!(forwarded.await, Err(AuthorizationError::TargetRedefined));
        assert_eq!(asked(), 5);
        // Journaled without the router, so nothing revokes what it replaces.
        let redefined = router.targets(|targets| targets.upsert(target("build", None)));
        let upserted = SessionEvent::TargetsUpserted {
            targets: vec![redefined.unwrap()],
        };
        (router.store)
            .append(caller.agent.clone(), upserted)
            .await
            .unwrap();
        assert_eq!(authorize(&path("deep")).await.unwrap(), 6);
    }
}

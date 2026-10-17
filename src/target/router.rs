use std::{path::Path, sync::Arc};

use serde::{Deserialize, Serialize};

use crate::{
    execution::ExecutionLocation,
    remote::{PreparedConnection, RemoteError, RemoteManager},
    session::{Command, CoreState, Plan, SessionError, SessionEvent, SessionStore},
    tool::{
        ToolError,
        authorization::{AuthorizationCoordinator, AuthorizationError, AuthorizationSubject},
        diagnostic::{Effects, FailureSite, Operation, PartialContext, Subject},
        policy::{Capability, CapabilitySet, PermissionUse, ResourceId},
    },
};

use super::{TargetDefinition, TargetError, TargetName, TargetRef, TargetRegistry};

/// The targets an SSH connection passes through, outermost first, and its
/// destination: a snapshot; the registry may since have changed.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub(crate) struct Route {
    pub hops: Vec<TargetDefinition>,
    pub destination: TargetDefinition,
}

impl Route {
    pub fn iter(&self) -> impl Iterator<Item = &TargetDefinition> {
        self.hops.iter().chain([&self.destination])
    }

    /// Each target on the route with its revision, outermost first.
    pub fn identity(&self) -> super::Revisions {
        self.iter()
            .map(|hop| (hop.name.clone(), hop.revision))
            .collect()
    }

    /// Connecting needs target approval, plus ssh_agent approval when any hop
    /// forwards an agent Skyhook does not own.
    pub fn permissions(&self) -> Vec<PermissionUse> {
        let resource = ResourceId::route(self.destination.name.clone(), self.identity());
        let external = self.iter().any(|hop| hop.ssh.external_agent);
        [Capability::Targets]
            .into_iter()
            .chain(external.then_some(Capability::SshAgent))
            .map(|capability| PermissionUse::exact(capability, resource.clone()))
            .collect()
    }

    pub fn authorization_arguments(&self) -> serde_json::Value {
        let destination = &self.destination;
        let names = |external: bool| {
            self.iter()
                .filter(|hop| !external || hop.ssh.external_agent)
                .map(|hop| &hop.name)
                .collect::<Vec<_>>()
        };
        serde_json::json!({
            "destination": destination.name,
            "host": destination.host,
            "origin": destination.origin,
            "route": names(false),
            "external_agent": names(true),
        })
    }
}

#[derive(Clone)]
pub(crate) struct TargetRouter {
    store: SessionStore,
    remote: RemoteManager,
    authorization: AuthorizationCoordinator,
}

/// A registration's failure to commit: a refused or indeterminate append.
struct Unsaved(RemoteError);

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
        let error = RemoteError::Session(Arc::new(error));
        Self(registration_error(error, Operation::Save, effects))
    }
}

impl TargetRouter {
    pub fn new(
        store: SessionStore,
        remote: RemoteManager,
        authorization: AuthorizationCoordinator,
    ) -> Self {
        Self {
            store,
            remote,
            authorization,
        }
    }

    pub(crate) async fn environment(
        &self,
        authentication: crate::remote::backend::ProcessAuthentication,
    ) -> Result<crate::remote::backend::ProcessEnvironment, RemoteError> {
        self.remote.environment(authentication).await
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
    ) -> Result<TargetDefinition, RemoteError> {
        let rejected = |error: TargetError| {
            Unsaved(registration_error(
                error,
                Operation::Validate,
                Effects::Unchanged,
            ))
        };
        let (agent, cancellation) = (subject.agent.clone(), subject.cancellation.clone());
        let ssh_agent = subject.capabilities.contains(Capability::SshAgent);
        let router = self.clone();
        let upsert = move |state: &CoreState| {
            let targets = &state.targets;
            // Without ssh_agent, targets reached through an external agent are
            // invisible: they can be neither replaced nor used as a via or origin.
            if !ssh_agent {
                if targets.needs_ssh_agent(&definition.name) {
                    let name = definition.name.clone();
                    return Err(rejected(TargetError::NameUnavailable(name)));
                }
                if let Some((edge, parent)) = definition.parent_edge()
                    && targets.needs_ssh_agent(parent)
                {
                    return Err(rejected(TargetError::UnknownReference {
                        target: definition.name.clone(),
                        edge,
                        reference: parent.clone(),
                    }));
                }
            }
            let targets = targets.upsert(vec![definition]).map_err(rejected)?;
            if cancellation.is_cancelled() {
                let cancelled = RemoteError::Cancelled;
                let error = registration_error(cancelled, Operation::Prepare, Effects::Unchanged);
                return Err(Unsaved(error));
            }
            let destination = targets[0].clone();
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

    /// Read the session's targets.
    pub(crate) fn targets<T>(&self, read: impl FnOnce(&TargetRegistry) -> T) -> T {
        self.store.read(|state| read(&state.targets))
    }

    /// Resolve a target visible with `capabilities`: without ssh_agent, a target whose
    /// route forwards an external agent is unknown.
    pub fn resolve(
        &self,
        target: &TargetName,
        capabilities: &CapabilitySet,
    ) -> Result<Route, TargetError> {
        let route = self.targets(|targets| targets.route(target))?;
        if !capabilities.contains(Capability::SshAgent)
            && route.iter().any(|hop| hop.ssh.external_agent)
        {
            return Err(TargetError::Unknown(target.clone()));
        }
        Ok(route)
    }

    /// Connect along `route`, admitted only while every target on it is at the
    /// revision it was planned for, read against the session's targets as its
    /// pooled startup is joined or started, and once it connected. A replaced
    /// revision, or a startup a redefinition stopped, is refused as redefined:
    /// its call plans again.
    pub async fn prepare(
        &self,
        route: Route,
        workspace: &Path,
        subject: &AuthorizationSubject,
    ) -> Result<PreparedConnection, RemoteError> {
        let redefined = || RemoteError::from(AuthorizationError::TargetRedefined);
        let current = || {
            let current = self.resolve(&route.destination.name, &subject.capabilities)?;
            (current.identity() == route.identity())
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
fn registration_error(
    error: impl Into<RemoteError>,
    operation: Operation,
    effects: Effects,
) -> RemoteError {
    error.into().or(
        PartialContext::new(operation, Subject::Label("target registration".into()))
            .at(FailureSite::Host)
            .effects(effects),
    )
}

#[cfg(test)]
impl TargetRouter {
    /// A router over `store`'s targets, with `definitions` added, whose
    /// connections use the default factory.
    pub(crate) async fn test(
        store: &SessionStore,
        definitions: impl IntoIterator<Item = TargetDefinition>,
        policy: Arc<dyn crate::tool::policy::Policy>,
    ) -> Self {
        let remote = RemoteManager::new(
            crate::remote::EmbeddedShimCatalog::default(),
            Arc::new(crate::remote::RejectSensitivePrompts),
            None,
            Default::default(),
        );
        let targets = definitions.into_iter().collect();
        let agent = crate::identity::AgentId::root(store.id());
        let upserted = SessionEvent::TargetsUpserted { targets };
        store.append(agent, upserted).await.unwrap();
        let authorization = AuthorizationCoordinator::new(policy).with_targets(store.clone());
        Self::new(store.clone(), remote, authorization)
    }

    /// Connect with `factory` instead.
    pub(crate) fn with_connection_factory(
        mut self,
        factory: Arc<dyn crate::remote::ConnectionFactory>,
    ) -> Self {
        self.remote = self.remote.with_connection_factory(factory);
        self
    }

    pub(crate) fn authorization(&self) -> &AuthorizationCoordinator {
        &self.authorization
    }
}

/// Where a target-relative operation runs, with the route to a named target
/// as resolved for the selection. The route is a snapshot, not evidence of
/// continued registry freshness.
pub(crate) struct SelectedLocation {
    pub(crate) location: ExecutionLocation,
    pub(crate) route: Option<Route>,
}

/// Select where a target-relative operation runs: the caller's location, or
/// an explicit target, which requires the targets capability. The root runs
/// in the root workspace; another target in its configured workspace, but
/// naming the caller's own target keeps the caller's workspace.
pub(crate) fn select_location(
    caller: &ExecutionLocation,
    root_workspace: &Path,
    explicit: Option<&TargetRef>,
    capabilities: &CapabilitySet,
    router: Option<&TargetRouter>,
) -> Result<SelectedLocation, ToolError> {
    if explicit.is_some() && !capabilities.contains(Capability::Targets) {
        return Err(ToolError::invalid_arguments(
            "target selection requires the targets capability",
        ));
    }
    let Some(name) = explicit.unwrap_or(&caller.target).name() else {
        let location = match explicit {
            Some(_) => ExecutionLocation::root(root_workspace.to_owned()),
            None => caller.clone(),
        };
        return Ok(SelectedLocation {
            location,
            route: None,
        });
    };
    let router = router.ok_or_else(|| {
        ToolError::invalid_arguments("remote targets are unavailable in this tool runtime")
    })?;
    let route = router
        .resolve(name, capabilities)
        .map_err(|error| ToolError::from(error.into_admission_error()))?;
    let location = if caller.target.name() == Some(name) {
        caller.clone()
    } else {
        let destination = &route.destination;
        ExecutionLocation::named(destination.name.clone(), destination.workspace.clone())
    };
    Ok(SelectedLocation {
        location,
        route: Some(route),
    })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex as StdMutex, atomic::Ordering},
    };

    use super::*;
    use crate::{
        identity::{AgentId, JobId, SessionId},
        job::CancellationToken,
        remote::{ConnectionFactory, ShimFactory},
        tests::RecordingPolicy,
        tool::{
            authorization::Asking,
            diagnostic::{Cause, PartialDiagnostic},
            policy::PolicyDecision,
        },
    };

    /// Replays scripted decisions, then allows; empty allowances adopt the proposed grants.
    fn recording(decisions: impl IntoIterator<Item = PolicyDecision>) -> Arc<RecordingPolicy> {
        let decisions = StdMutex::new(decisions.into_iter().collect::<VecDeque<_>>());
        RecordingPolicy::deciding(move |request| {
            let mut decision = decisions
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(PolicyDecision::allow);
            if let PolicyDecision::Allow { grants } = &mut decision
                && grants.is_empty()
            {
                *grants = request
                    .permissions
                    .iter()
                    .filter_map(PermissionUse::proposed_grant)
                    .collect();
            }
            decision
        })
    }

    fn deny(reason: &str) -> PolicyDecision {
        PolicyDecision::Deny {
            reason: reason.into(),
        }
    }

    fn target(name: &str, via: Option<&str>) -> TargetDefinition {
        TargetDefinition::test(name, format!("/{name}"), via)
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
        let mut gateway = target("gateway", None);
        gateway.revision = 7;
        let mut destination = target("build", Some("gateway"));
        destination.revision = 19;
        let mut route = Route {
            hops: vec![gateway],
            destination,
        };
        let hops = vec![(name("gateway"), 7), (name("build"), 19)];
        let resource = ResourceId::route(name("build"), hops);
        let permission = |capability| PermissionUse::exact(capability, resource.clone());
        assert_eq!(route.permissions(), [permission(Capability::Targets)]);
        let arguments = route.authorization_arguments();
        assert_eq!(arguments["route"], serde_json::json!(["gateway", "build"]));
        // Forwarding an external agent through any hop needs its own approval.
        route.hops[0].ssh.external_agent = true;
        let expected = [Capability::Targets, Capability::SshAgent].map(permission);
        assert_eq!(route.permissions(), expected);
        let arguments = route.authorization_arguments();
        assert_eq!(arguments["external_agent"], serde_json::json!(["gateway"]));
    }

    #[tokio::test]
    async fn targets_needing_ssh_agent_are_invisible_without_it() {
        let mut external = target("external", None);
        external.ssh.external_agent = true;
        let targets = [external, target("behind", Some("external"))];
        let (_root, router) = router(targets, recording([]), None).await;
        let without = CapabilitySet::default();
        let mut with = without.clone();
        with.insert(Capability::SshAgent);
        for hidden in ["external", "behind"] {
            let resolved = router.resolve(&name(hidden), &without);
            assert!(matches!(resolved, Err(TargetError::Unknown(_))), "{hidden}");
            router.resolve(&name(hidden), &with).unwrap();
        }
        assert!(router.targets(|targets| targets.list(&without)).is_empty());
        assert_eq!(router.targets(|targets| targets.list(&with)).len(), 2);
        // Hidden targets can be neither replaced nor routed through.
        let subject = subject_for(&router.store);
        let mut via = target("new", None);
        via.via = Some(name("behind"));
        let mut origin = target("new", None);
        origin.origin = Some(name("behind"));
        for (definition, argument) in [
            (target("external", None), "name"),
            (via, "via"),
            (origin, "origin"),
        ] {
            let error = router.add(definition, &subject).await.unwrap_err();
            let diagnostic = error.into_tool_error().diagnostic();
            assert!(matches!(diagnostic.cause, Cause::InvalidArguments(_)));
            assert_eq!(diagnostic.context.subject, Subject::argument([argument]));
            assert_eq!(diagnostic.context.site, FailureSite::Host);
            assert_eq!(diagnostic.context.effects, Effects::Unchanged);
        }
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
        let route = router
            .resolve(&name("build"), &CapabilitySet::default())
            .unwrap();
        approve(router, &route, subject).await?;
        router.prepare(route, Path::new("/override"), subject).await
    }

    async fn approve(
        router: &TargetRouter,
        route: &Route,
        subject: &AuthorizationSubject,
    ) -> Result<(), RemoteError> {
        let permissions = route.permissions();
        let arguments = route.authorization_arguments();
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
    async fn denial_is_retried_and_jump_updates_change_route_identity() {
        let policy = recording([deny("no"), PolicyDecision::allow(), PolicyDecision::allow()]);
        let targets = definitions(&[("gateway", None), ("build", Some("gateway"))]);
        let (_root, router) = router(targets, policy.clone(), None).await;
        let subject = subject();
        let original = router
            .resolve(&name("build"), &CapabilitySet::default())
            .unwrap();
        assert!(matches!(
            approve(&router, &original, &subject).await,
            Err(RemoteError::Authorization(AuthorizationError::Denied(reason))) if reason.contains("no")
        ));
        approve(&router, &original, &subject).await.unwrap();

        replace_target(&router, target("gateway", None)).await;
        let changed = router
            .resolve(&name("build"), &CapabilitySet::default())
            .unwrap();
        assert_ne!(original.identity(), changed.identity());
        approve(&router, &changed, &subject).await.unwrap();
        assert_eq!(policy.requests.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn admitted_route_snapshot_survives_later_registry_mutation() {
        let factory = ShimFactory::new();
        let (_root, router) = router(
            definitions(&[("build", None)]),
            recording([]),
            Some(factory.clone()),
        )
        .await;
        let admitted = router
            .resolve(&name("build"), &CapabilitySet::default())
            .unwrap();
        factory.ready.add_permits(1);
        let prepared = prepare(&router, &subject()).await.unwrap();
        assert!(router.remote.is_current(&prepared));

        replace_target(&router, target("build", None)).await;
        let current = router
            .resolve(&name("build"), &CapabilitySet::default())
            .unwrap();
        assert_ne!(admitted.identity(), current.identity());
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
            let policy = recording([]);
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
                let stale = (router.resolve(&name("build"), &CapabilitySet::default())).unwrap();
                approve(&router, &stale, &subject).await.unwrap();
                replace_target(&router, target("gateway", None)).await;
                crate::tests::bounded(router.prepare(stale, Path::new("/override"), &subject)).await
            };
            assert!(matches!(
                prepared,
                Err(RemoteError::Authorization(
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
    async fn cancelled_registration_changes_nothing_and_never_connects() {
        let factory = ShimFactory::new();
        let (_root, router) = router([], recording([]), Some(factory.clone())).await;
        let subject = subject_for(&router.store);
        subject.cancellation.cancel();
        let error = router
            .add(target("cancelled", None), &subject)
            .await
            .unwrap_err();
        let diagnostic = error.into_tool_error().diagnostic();
        assert_eq!(diagnostic.cause, Cause::Cancelled);
        assert_eq!(diagnostic.context.site, FailureSite::Host);
        assert_eq!(diagnostic.context.effects, Effects::Unchanged);
        let added = router.targets(|targets| targets.get(&name("cancelled")));
        assert!(added.is_err());
        assert_eq!(factory.starts.load(Ordering::SeqCst), 0);
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
            let diagnostic = error.into_tool_error().diagnostic();
            assert_eq!(diagnostic.context.operation, Operation::Save);
            assert_eq!(diagnostic.context.effects, effects);
        }
        // A mismatched session rejects append after normalization and graph validation.
        let (_root, router) = router(definitions(&[("first", None)]), recording([]), None).await;
        let error = router
            .add(target("second", None), &subject())
            .await
            .unwrap_err();
        let diagnostic = error.into_tool_error().diagnostic();
        assert_eq!(
            diagnostic.cause,
            PartialDiagnostic::session(&SessionError::WrongSession).cause
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
        let policy = recording([]);
        let targets = [target("build", None), target("deep", Some("build"))];
        let (_root, router) = router(targets, policy.clone(), None).await;
        let mut caller = subject_for(&router.store);
        caller.capabilities.insert(Capability::Write);
        let path = crate::tool::policy::PathText::new("/src").unwrap();
        let path = |target: &str| ResourceId::path(&target.parse().unwrap(), &path);
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
        let stale = (router.resolve(&name("build"), &CapabilitySet::default())).unwrap();
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
        assert!(matches!(
            route.await,
            Err(AuthorizationError::TargetRedefined)
        ));
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
        assert!(matches!(
            forwarded.await,
            Err(AuthorizationError::TargetRedefined)
        ));
        assert_eq!(asked(), 5);
        // Journaled without the router, so nothing revokes what it replaces.
        let redefined = router.targets(|targets| targets.upsert(vec![target("build", None)]));
        let upserted = SessionEvent::TargetsUpserted {
            targets: redefined.unwrap(),
        };
        (router.store)
            .append(caller.agent.clone(), upserted)
            .await
            .unwrap();
        assert_eq!(authorize(&path("deep")).await.unwrap(), 6);
    }

    #[tokio::test]
    async fn selection_resolves_inherit_root_and_other_targets() {
        let root = Path::new("/root");
        let build = ExecutionLocation::named("build".parse().unwrap(), "/caller-override".into());
        let overridden = ExecutionLocation::root("/override".into());
        let session = crate::tests::TestRuntime::new().await;
        let targets = [
            TargetDefinition::test("build", "/configured", None),
            TargetDefinition::test("other", "relative directory/../project", None),
        ];
        let policy = crate::tests::RecordingPolicy::allowing();
        let router = TargetRouter::test(&session.store, targets, policy).await;
        let mut capabilities = CapabilitySet::default();
        capabilities.insert(Capability::Targets);
        let [root_target, current, other] =
            ["root", "build", "other"].map(|name| name.parse::<TargetRef>().unwrap());
        let select = |caller, explicit, capabilities| {
            select_location(caller, root, explicit, capabilities, Some(&router))
        };
        for (caller, explicit, expected) in [
            (&build, None, build.clone()),
            (&overridden, None, overridden.clone()),
            (
                &build,
                Some(&root_target),
                ExecutionLocation::root("/root".into()),
            ),
            (
                &overridden,
                Some(&root_target),
                ExecutionLocation::root("/root".into()),
            ),
            // An explicit current target keeps the caller's workspace override.
            (&build, Some(&current), build.clone()),
            (
                &build,
                Some(&other),
                ExecutionLocation::named(
                    "other".parse().unwrap(),
                    "relative directory/../project".into(),
                ),
            ),
        ] {
            let selected = select(caller, explicit, &capabilities).unwrap();
            assert_eq!(selected.location, expected);
            assert_eq!(
                selected.route.map(|route| route.destination.name.clone()),
                expected.target.name().cloned()
            );
        }
        // Naming any target needs the capability; inheriting one does not.
        let without = CapabilitySet::default();
        assert!(select(&overridden, Some(&root_target), &without).is_err());
        assert!(select(&build, None, &without).is_ok());
        let unknown = "missing".parse::<TargetRef>().unwrap();
        assert!(select(&build, Some(&unknown), &capabilities).is_err());
    }
}

use std::{path::Path, sync::Arc};

use tokio::sync::RwLock;

use crate::{
    execution::ExecutionLocation,
    remote::{PreparedConnection, RemoteError, RemoteManager},
    tool::{
        ToolError,
        authorization::{AuthorizationCoordinator, AuthorizationSubject},
        diagnostic::{Effects, FailureSite, Operation, PartialContext, Subject},
        policy::{Capability, CapabilitySet, PermissionUse, ResourceId},
    },
};

use super::{TargetDefinition, TargetError, TargetName, TargetRef, TargetRegistry};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RouteIdentity {
    destination: TargetName,
    pub hops: Vec<(String, u64)>,
}

impl RouteIdentity {
    pub fn destination(&self) -> &TargetName {
        &self.destination
    }
}

/// A nonempty route snapshot; the registry may since have changed.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedRoute {
    identity: RouteIdentity,
    definitions: Vec<TargetDefinition>,
}

impl ResolvedRoute {
    /// Captures already validated definitions; `None` when empty.
    pub fn from_definitions(definitions: Vec<TargetDefinition>) -> Option<Self> {
        let destination = definitions.last()?.name.clone();
        let hops = definitions
            .iter()
            .map(|hop| (hop.name.to_string(), hop.revision))
            .collect();
        Some(Self {
            identity: RouteIdentity { destination, hops },
            definitions,
        })
    }

    pub fn identity(&self) -> &RouteIdentity {
        &self.identity
    }

    pub fn definitions(&self) -> &[TargetDefinition] {
        &self.definitions
    }

    pub fn destination(&self) -> &TargetDefinition {
        self.definitions
            .last()
            .expect("resolved routes contain a destination")
    }

    /// Connecting needs target approval, plus ssh_agent approval when any hop
    /// forwards an agent Skyhook does not own.
    pub fn permissions(&self) -> Vec<PermissionUse> {
        let resource = ResourceId::route(
            self.identity.destination.to_string(),
            self.identity.hops.clone(),
        );
        let external = self.definitions.iter().any(|hop| hop.ssh.external_agent);
        [Capability::Targets]
            .into_iter()
            .chain(external.then_some(Capability::SshAgent))
            .map(|capability| PermissionUse::exact(capability, resource.clone()))
            .collect()
    }

    pub fn authorization_arguments(&self) -> serde_json::Value {
        let destination = self.destination();
        let names = |external: bool| {
            (self.definitions.iter())
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
    targets: TargetRegistry,
    remote: RemoteManager,
    authorization: AuthorizationCoordinator,
    mutation: Arc<RwLock<()>>,
}

impl TargetRouter {
    pub fn new(
        targets: TargetRegistry,
        remote: RemoteManager,
        authorization: AuthorizationCoordinator,
    ) -> Self {
        Self {
            targets,
            remote,
            authorization,
            mutation: Arc::new(RwLock::new(())),
        }
    }

    pub(crate) async fn environment(
        &self,
    ) -> Result<crate::remote::backend::ProcessEnvironment, RemoteError> {
        self.remote.environment().await
    }
    pub(crate) async fn shutdown(&self) {
        // Drain accepted registrations, which hold this gate until published.
        let _mutation = self.mutation.write().await;
        self.remote.shutdown().await;
    }

    pub(crate) async fn add(
        &self,
        definition: TargetDefinition,
        subject: &AuthorizationSubject,
        store: &crate::session::SessionStore,
    ) -> Result<TargetDefinition, RemoteError> {
        let rejected =
            |error: TargetError| registration_error(error, Operation::Validate, Effects::Unchanged);
        let mutation = self.mutation.clone().write_owned().await;
        // Without ssh_agent, targets reached through an external agent are invisible:
        // they can be neither replaced nor used as a via or origin. The mutation gate
        // keeps the registry unchanged until staging.
        if !subject.capabilities.contains(Capability::SshAgent) {
            if self.targets.needs_ssh_agent(&definition.name).await {
                return Err(rejected(TargetError::NameUnavailable(
                    definition.name.to_string(),
                )));
            }
            if let Some((edge, parent)) = definition.parent_edge()
                && self.targets.needs_ssh_agent(parent).await
            {
                return Err(rejected(TargetError::UnknownReference {
                    target: definition.name.to_string(),
                    edge,
                    reference: parent.to_string(),
                }));
            }
        }
        let prepared = self
            .targets
            .prepare_upsert_many(vec![definition])
            .await
            .map_err(rejected)?;
        if subject.cancellation.is_cancelled() {
            return Err(registration_error(
                RemoteError::Cancelled,
                Operation::Prepare,
                Effects::Unchanged,
            ));
        }
        // The staged definition carries its allocated revision.
        let targets = prepared.definitions().to_vec();
        let destination = targets[0].clone();
        let store = store.clone();
        let agent = subject.agent.clone();
        let router = self.clone();
        // The spawned owner finishes journal append and publication even if the
        // caller is dropped; a failed append publishes nothing.
        tokio::spawn(async move {
            let _mutation = mutation;
            store
                .append(
                    agent,
                    crate::session::SessionEvent::TargetsUpserted { targets },
                )
                .await
                .map_err(|error| {
                    let effects = match &error {
                        crate::session::SessionError::AppendIndeterminate(_) => Effects::Unknown,
                        _ => Effects::Unchanged,
                    };
                    registration_error(
                        RemoteError::Session(Arc::new(error)),
                        Operation::Save,
                        effects,
                    )
                })?;
            let (_, invalidated) = prepared.publish().await;
            router
                .authorization
                .revoke(|grant| {
                    matches!(&grant.resource, ResourceId::Route { destination, .. }
                        if invalidated.iter().any(|name| name.as_str() == destination))
                })
                .await;
            router.remote.invalidate(&invalidated).await;
            Ok(destination)
        })
        .await
        .map_err(|_| {
            registration_error(
                RemoteError::ConnectionTask("target publication owner lost".into()),
                Operation::Wait,
                Effects::MayHaveExecuted,
            )
        })?
    }

    pub fn targets(&self) -> &TargetRegistry {
        &self.targets
    }

    /// Resolve a target visible with `capabilities`: without ssh_agent, a target whose
    /// route forwards an external agent is unknown.
    pub async fn resolve(
        &self,
        target: &TargetName,
        capabilities: &CapabilitySet,
    ) -> Result<ResolvedRoute, TargetError> {
        let definitions = self.targets.route(target).await?;
        if !capabilities.contains(Capability::SshAgent)
            && definitions.iter().any(|hop| hop.ssh.external_agent)
        {
            return Err(TargetError::Unknown(target.to_string()));
        }
        // Successful registry resolution always includes the requested destination.
        Ok(ResolvedRoute::from_definitions(definitions)
            .expect("target registry resolves a nonempty route"))
    }

    pub async fn prepare(
        &self,
        mut route: ResolvedRoute,
        workspace: &Path,
        subject: &AuthorizationSubject,
    ) -> Result<PreparedConnection, RemoteError> {
        loop {
            let prepared = self
                .remote
                .connection(route.clone(), workspace, &subject.cancellation)
                .await?;
            let mutation = self.mutation.read().await;
            let current =
                (self.resolve(route.identity.destination(), &subject.capabilities)).await?;
            if current.identity() == route.identity() && self.remote.is_current(&prepared).await {
                // Admitted: later registry mutation does not revoke this snapshot.
                drop(mutation);
                return Ok(prepared);
            }
            drop(mutation);
            self.remote.discard(&prepared).await;
            route = current;
            self.authorization
                .authorize(
                    subject,
                    "connect_target".to_owned(),
                    route.permissions(),
                    route.authorization_arguments(),
                )
                .await?;
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
    /// A router over `targets` whose connections use the default factory.
    pub(crate) fn test(
        targets: TargetRegistry,
        policy: Arc<dyn crate::tool::policy::Policy>,
    ) -> Self {
        let authorization = AuthorizationCoordinator::new(policy);
        let remote = RemoteManager::new(
            crate::remote::EmbeddedShimCatalog::default(),
            Arc::new(crate::remote::RejectSensitivePrompts),
            authorization.clone(),
        );
        Self::new(targets, remote, authorization)
    }
}

/// Where a target-relative operation runs, with the route to a named target
/// as resolved for the selection. The route is a snapshot, not evidence of
/// continued registry freshness.
pub(crate) struct SelectedLocation {
    pub(crate) location: ExecutionLocation,
    pub(crate) route: Option<ResolvedRoute>,
}

/// Select where a target-relative operation runs: the caller's location, or
/// an explicit target, which requires the targets capability. The root runs
/// in the root workspace; another target in its configured workspace, but
/// naming the caller's own target keeps the caller's workspace.
pub(crate) async fn select_location(
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
        .await
        .map_err(|error| ToolError::from(error.into_admission_error()))?;
    let location = if caller.target.name() == Some(name) {
        caller.clone()
    } else {
        let destination = route.destination();
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
        remote::{
            ConnectionFactory, EmbeddedShimCatalog, PendingHandshakeFactory, RejectSensitivePrompts,
        },
        session::{AppendBoundary, SessionStore},
        tests::RecordingPolicy,
        tool::authorization::AuthorizationError,
        tool::{
            diagnostic::{Cause, PartialDiagnostic},
            policy::PolicyDecision,
        },
    };

    const BOUNDARIES: [AppendBoundary; 2] = [AppendBoundary::Write, AppendBoundary::Publication];

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

    fn registry(targets: &[(&str, Option<&str>)]) -> TargetRegistry {
        TargetRegistry::from_definitions(targets.iter().map(|(name, via)| target(name, *via)))
            .unwrap()
    }

    #[test]
    fn resolved_route_derives_destination_and_identity_from_owned_definitions() {
        let mut gateway = target("gateway", None);
        gateway.revision = 7;
        let mut destination = target("build", Some("gateway"));
        destination.revision = 19;
        let definitions = vec![gateway, destination];
        let route = ResolvedRoute::from_definitions(definitions.clone()).unwrap();
        assert_eq!(route.definitions(), definitions.as_slice());
        assert_eq!(route.destination(), &definitions[1]);
        assert_eq!(route.identity.destination(), &name("build"));
        let hops = vec![("gateway".into(), 7), ("build".into(), 19)];
        assert_eq!(route.identity.hops, hops);
        let resource = ResourceId::route("build", hops);
        let permission = |capability| PermissionUse::exact(capability, resource.clone());
        assert_eq!(route.permissions(), [permission(Capability::Targets)]);
        let arguments = route.authorization_arguments();
        assert_eq!(arguments["route"], serde_json::json!(["gateway", "build"]));
        // Forwarding an external agent through any hop needs its own approval.
        let mut definitions = definitions;
        definitions[0].ssh.external_agent = true;
        let route = ResolvedRoute::from_definitions(definitions).unwrap();
        let expected = [Capability::Targets, Capability::SshAgent].map(permission);
        assert_eq!(route.permissions(), expected);
        let arguments = route.authorization_arguments();
        assert_eq!(arguments["external_agent"], serde_json::json!(["gateway"]));
    }

    #[tokio::test]
    async fn targets_needing_ssh_agent_are_invisible_without_it() {
        let mut external = target("external", None);
        external.ssh.external_agent = true;
        let targets =
            TargetRegistry::from_definitions([external, target("behind", Some("external"))]);
        let router = router(targets.unwrap(), recording([]), None);
        let without = CapabilitySet::default();
        let mut with = without.clone();
        with.insert(Capability::SshAgent);
        for hidden in ["external", "behind"] {
            let resolved = router.resolve(&name(hidden), &without).await;
            assert!(matches!(resolved, Err(TargetError::Unknown(_))), "{hidden}");
            router.resolve(&name(hidden), &with).await.unwrap();
        }
        let listed = |records: Vec<crate::target::TargetRecord>| {
            records
                .into_iter()
                .map(|record| match record {
                    crate::target::TargetRecord::Root => "root".to_owned(),
                    crate::target::TargetRecord::Ssh { name, .. } => name.to_string(),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(listed(router.targets.list(&without).await), ["root"]);
        assert_eq!(listed(router.targets.list(&with).await).len(), 3);
        // Hidden targets can be neither replaced nor routed through.
        let (_directory, store) = ephemeral_store().await;
        let subject = subject_for(&store);
        let mut via = target("new", None);
        via.via = Some(name("behind"));
        let mut origin = target("new", None);
        origin.origin = Some(name("behind"));
        for (definition, argument) in [
            (target("external", None), "name"),
            (via, "via"),
            (origin, "origin"),
        ] {
            let error = router.add(definition, &subject, &store).await.unwrap_err();
            let diagnostic = error.into_tool_error().diagnostic();
            assert!(matches!(diagnostic.cause, Cause::InvalidArguments(_)));
            assert_eq!(diagnostic.context.subject, Subject::argument([argument]));
            assert_eq!(diagnostic.context.site, FailureSite::Host);
            assert_eq!(diagnostic.context.effects, Effects::Unchanged);
        }
    }

    async fn ephemeral_store() -> (tempfile::TempDir, SessionStore) {
        let session = crate::session::tests::MemorySession::new().await;
        (session.root, session.store)
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
        let (_directory, store) = ephemeral_store().await;
        let subject = subject_for(&store);
        router.add(definition, &subject, &store).await.unwrap();
    }

    fn router(
        targets: TargetRegistry,
        policy: Arc<RecordingPolicy>,
        factory: Option<Arc<dyn ConnectionFactory>>,
    ) -> TargetRouter {
        let authorization = AuthorizationCoordinator::new(policy);
        let catalog = EmbeddedShimCatalog::default();
        let prompts = Arc::new(RejectSensitivePrompts);
        let mut remote = RemoteManager::new(catalog, prompts, authorization.clone());
        if let Some(factory) = factory {
            remote = remote.with_connection_factory(factory);
        }
        TargetRouter::new(targets, remote, authorization)
    }

    async fn prepare(
        router: &TargetRouter,
        subject: &AuthorizationSubject,
    ) -> Result<PreparedConnection, RemoteError> {
        let route = router
            .resolve(&name("build"), &CapabilitySet::default())
            .await
            .unwrap();
        approve(router, &route, subject).await?;
        router.prepare(route, Path::new("/override"), subject).await
    }

    async fn approve(
        router: &TargetRouter,
        route: &ResolvedRoute,
        subject: &AuthorizationSubject,
    ) -> Result<(), RemoteError> {
        let permissions = route.permissions();
        let arguments = route.authorization_arguments();
        let authorize = router.authorization.authorize(
            subject,
            "connect_target".to_owned(),
            permissions,
            arguments,
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
        let targets = registry(&[("gateway", None), ("build", Some("gateway"))]);
        let router = router(targets, policy.clone(), None);
        let subject = subject();
        let original = router
            .resolve(&name("build"), &CapabilitySet::default())
            .await
            .unwrap();
        assert!(matches!(
            approve(&router, &original, &subject).await,
            Err(RemoteError::Authorization(AuthorizationError::Denied(reason))) if reason.contains("no")
        ));
        approve(&router, &original, &subject).await.unwrap();

        replace_target(&router, target("gateway", None)).await;
        let changed = router
            .resolve(&name("build"), &CapabilitySet::default())
            .await
            .unwrap();
        assert_ne!(original.identity(), changed.identity());
        approve(&router, &changed, &subject).await.unwrap();
        assert_eq!(policy.requests.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn admitted_route_snapshot_survives_later_registry_mutation() {
        let factory = PendingHandshakeFactory::new();
        let router = router(
            registry(&[("build", None)]),
            recording([]),
            Some(factory.clone()),
        );
        let admitted = router
            .resolve(&name("build"), &CapabilitySet::default())
            .await
            .unwrap();
        factory.ready.add_permits(1);
        let prepared = prepare(&router, &subject()).await.unwrap();
        assert!(router.remote.is_current(&prepared).await);

        replace_target(&router, target("build", None)).await;
        let current = router
            .resolve(&name("build"), &CapabilitySet::default())
            .await
            .unwrap();
        assert_ne!(admitted.identity(), current.identity());
        assert_eq!(admitted.destination().revision, 1);
        assert_eq!(admitted.identity.hops, [("build".into(), 1)]);
        // Invalidation removes pool reuse; it does not consume the admitted handle.
        assert!(!router.remote.is_current(&prepared).await);
        assert_eq!(factory.starts.load(Ordering::SeqCst), 1);
        drop(prepared);
        router.shutdown().await;
    }

    #[tokio::test]
    async fn grant_revocation_after_authorization_does_not_reauthorize_unchanged_route() {
        let policy = recording([PolicyDecision::allow(), deny("revoked")]);
        let factory = PendingHandshakeFactory::new();
        let router = router(
            registry(&[("build", None)]),
            policy.clone(),
            Some(factory.clone()),
        );
        let subject = subject();
        let route = router
            .resolve(&name("build"), &CapabilitySet::default())
            .await
            .unwrap();
        approve(&router, &route, &subject).await.unwrap();
        router.authorization.revoke(|_| true).await;
        factory.ready.add_permits(1);
        let prepared = router
            .prepare(route.clone(), Path::new("/override"), &subject)
            .await;
        assert_eq!(policy.requests.lock().unwrap().len(), 1);
        assert!(matches!(approve(&router, &route, &subject).await,
            Err(RemoteError::Authorization(AuthorizationError::Denied(reason))) if reason == "revoked"));
        drop(prepared.unwrap());
        router.shutdown().await;
    }

    #[tokio::test]
    async fn waiters_share_one_startup_and_approval_and_cancel_separately() {
        let policy = recording([]);
        let factory = PendingHandshakeFactory::new();
        let targets = registry(&[("build", None)]);
        let router = router(targets, policy.clone(), Some(factory.clone()));
        let cancelled = subject();
        let first = spawn_prepare(&router, cancelled.clone());
        factory.wait_for_hello().await;
        let second = spawn_prepare(&router, subject());
        cancelled.cancellation.cancel();
        assert!(matches!(first.await.unwrap(), Err(RemoteError::Cancelled)));
        factory.ready.add_permits(1);
        second.await.unwrap().unwrap();
        assert_eq!(factory.starts.load(Ordering::SeqCst), 1);
        assert_eq!(policy.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn invalidation_during_startup_retries_before_returning() {
        let policy = recording([]);
        let factory = PendingHandshakeFactory::new();
        let targets = registry(&[("gateway", None), ("build", Some("gateway"))]);
        let router = router(targets, policy.clone(), Some(factory.clone()));
        let preparing = spawn_prepare(&router, subject());
        factory.wait_for_hello().await;
        replace_target(&router, target("gateway", None)).await;
        factory.ready.add_permits(2);
        preparing.await.unwrap().unwrap();
        assert_eq!(factory.starts.load(Ordering::SeqCst), 2);
        assert_eq!(policy.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn accepted_target_publication_survives_lost_waiter_and_shutdown_drains() {
        for boundary in BOUNDARIES {
            let (_root, store) = ephemeral_store().await;
            let targets = registry(&[("build", None)]);
            let router = router(targets.clone(), recording([]), None);
            let subject = subject_for(&store);
            let (reached, resume) = store.pause_append_at(boundary).await;
            let waiter = tokio::spawn({
                let (router, store) = (router.clone(), store.clone());
                async move {
                    let definition = target("build", None);
                    router.add(definition, &subject, &store).await
                }
            });
            reached.await.unwrap();
            waiter.abort();
            assert!(waiter.await.unwrap_err().is_cancelled());
            let held = router.mutation.try_read().is_err();
            assert!(held, "accepted owner retains publication gate");
            let shutdown = tokio::spawn({
                let router = router.clone();
                async move { router.shutdown().await }
            });
            tokio::task::yield_now().await;
            assert!(!shutdown.is_finished(), "shutdown must drain publication");
            resume.send(()).unwrap();
            shutdown.await.unwrap();
            let saved = targets.get(&name("build")).await.unwrap();
            assert_eq!(saved.revision, 2);
            let records = store.records().await;
            assert!(matches!(&records.last().unwrap().event,
                crate::session::SessionEvent::TargetsUpserted { targets } if targets == &[saved]));
            // Router shutdown, like runtime shutdown, does not close the journal.
            assert!(store.drain().await.is_ok());
        }
    }

    #[tokio::test]
    async fn cancelled_registration_changes_nothing_and_never_connects() {
        let (_root, store) = ephemeral_store().await;
        let factory = PendingHandshakeFactory::new();
        let router = router(
            TargetRegistry::default(),
            recording([]),
            Some(factory.clone()),
        );
        let subject = subject_for(&store);
        subject.cancellation.cancel();
        let error = router
            .add(target("cancelled", None), &subject, &store)
            .await
            .unwrap_err();
        let diagnostic = error.into_tool_error().diagnostic();
        assert_eq!(diagnostic.cause, Cause::Cancelled);
        assert_eq!(diagnostic.context.site, FailureSite::Host);
        assert_eq!(diagnostic.context.effects, Effects::Unchanged);
        assert!(router.targets.get(&name("cancelled")).await.is_err());
        assert_eq!(factory.starts.load(Ordering::SeqCst), 0);
        router.shutdown().await;
    }

    #[tokio::test]
    async fn failed_or_indeterminate_target_append_releases_guards_without_publication() {
        for boundary in BOUNDARIES {
            let (_root, store) = ephemeral_store().await;
            let original = target("build", None);
            let targets = registry(&[("build", None)]);
            let router = router(targets.clone(), recording([]), None);
            store.fail_append_at(boundary).await;
            let error = router
                .add(target("build", None), &subject_for(&store), &store)
                .await
                .unwrap_err();
            let diagnostic = error.into_tool_error().diagnostic();
            assert_eq!(diagnostic.context.site, FailureSite::Host);
            assert_eq!(diagnostic.context.operation, Operation::Save);
            // No live publication occurred, but persistence may have committed.
            assert_eq!(diagnostic.context.effects, Effects::Unknown);
            assert_eq!(targets.get(&name("build")).await.unwrap(), original);
            assert!(router.mutation.try_write().is_ok());
            let recovery = match store.drain().await {
                Err(crate::session::SessionError::AppendUnavailable(recovery)) => recovery,
                result => panic!("poisoned writer must require recovery on drain: {result:?}"),
            };
            assert_eq!(recovery.identity.session, store.id());
            assert!(!recovery.reason.is_empty());
            assert_eq!(
                diagnostic.cause,
                PartialDiagnostic::session(&crate::session::SessionError::AppendIndeterminate(
                    recovery
                ))
                .cause
            );
            router.shutdown().await;
        }
        // A mismatched session rejects append after normalization and graph validation.
        let (_root, store) = ephemeral_store().await;
        let targets = registry(&[("first", None)]);
        let before = targets.definitions().await;
        let router = router(targets.clone(), recording([]), None);
        let error = router
            .add(target("second", None), &subject(), &store)
            .await
            .unwrap_err();
        let diagnostic = error.into_tool_error().diagnostic();
        assert_eq!(
            diagnostic.cause,
            PartialDiagnostic::session(&crate::session::SessionError::WrongSession).cause
        );
        assert_eq!(diagnostic.context.operation, Operation::Save);
        assert_eq!(diagnostic.context.site, FailureSite::Host);
        assert_eq!(diagnostic.context.effects, Effects::Unchanged);
        assert_eq!(targets.definitions().await, before);
    }

    #[tokio::test]
    async fn selection_resolves_inherit_root_and_other_targets() {
        let root = Path::new("/root");
        let build = ExecutionLocation::named("build".parse().unwrap(), "/caller-override".into());
        let overridden = ExecutionLocation::root("/override".into());
        let targets = TargetRegistry::from_definitions([
            TargetDefinition::test("build", "/configured", None),
            TargetDefinition::test("other", "relative directory/../project", None),
        ])
        .unwrap();
        let router = TargetRouter::test(targets, crate::tests::RecordingPolicy::allowing());
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
            let selected = select(caller, explicit, &capabilities).await.unwrap();
            assert_eq!(selected.location, expected);
            assert_eq!(
                selected.route.map(|route| route.destination().name.clone()),
                expected.target.name().cloned()
            );
        }
        // Naming any target needs the capability; inheriting one does not.
        let without = CapabilitySet::default();
        assert!(
            select(&overridden, Some(&root_target), &without)
                .await
                .is_err()
        );
        assert!(select(&build, None, &without).await.is_ok());
        let unknown = "missing".parse::<TargetRef>().unwrap();
        assert!(select(&build, Some(&unknown), &capabilities).await.is_err());
    }
}

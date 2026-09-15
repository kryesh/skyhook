use std::{path::Path, sync::Arc};

use tokio::sync::RwLock;

use crate::{
    remote::{PreparedConnection, RemoteError, RemoteManager},
    tool::{
        authorization::{AuthorizationCoordinator, AuthorizationSubject},
        policy::{ApprovalGrant, Capability, PermissionUse, ResourceId},
    },
};

use super::{TargetDefinition, TargetError, TargetRegistry};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RouteIdentity {
    destination: String,
    pub hops: Vec<(String, u64)>,
}

impl RouteIdentity {
    pub fn destination(&self) -> &str {
        &self.destination
    }
}

/// A nonempty immutable route snapshot with identity derived from its definitions.
///
/// This does not prove that the registry still contains the same revisions.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedRoute {
    identity: RouteIdentity,
    definitions: Vec<TargetDefinition>,
}

impl ResolvedRoute {
    /// Capture a nonempty route and derive its identity from the same definitions.
    ///
    /// Registry resolution remains responsible for graph/configuration validation.
    /// This is a snapshot, not a promise that the registry will remain unchanged.
    pub fn from_definitions(definitions: Vec<TargetDefinition>) -> Option<Self> {
        let destination = definitions.last()?.name.clone();
        let hops = definitions
            .iter()
            .map(|hop| (hop.name.clone(), hop.revision))
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

    pub fn permission(&self) -> PermissionUse {
        let resource = ResourceId::route(
            self.identity.destination.clone(),
            self.identity.hops.clone(),
        );
        PermissionUse::new(Capability::Targets, resource.clone())
            .with_grant(ApprovalGrant::exact(Capability::Targets, resource))
    }

    pub fn authorization_arguments(&self) -> serde_json::Value {
        let destination = self.destination();
        serde_json::json!({
            "destination": destination.name,
            "type": destination.r#type,
            "origin": destination.origin,
            "host": destination.host,
            "route": self.definitions().iter().map(|hop| &hop.name).collect::<Vec<_>>(),
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
    pub(crate) async fn authorize_transfer(
        &self,
        context: &crate::tool::ToolContext,
        tool: &str,
        permissions: Vec<PermissionUse>,
        arguments: serde_json::Value,
    ) -> Result<(), crate::tool::ToolError> {
        self.authorization
            .authorize(
                context.invocation_subject()?,
                tool.to_owned(),
                permissions,
                arguments,
            )
            .await
            .map_err(|error| match error {
                crate::tool::authorization::AuthorizationError::Cancelled => {
                    crate::tool::ToolError::Cancelled
                }
                crate::tool::authorization::AuthorizationError::Denied(reason) => {
                    crate::tool::ToolError::Denied(reason)
                }
                crate::tool::authorization::AuthorizationError::Unavailable => {
                    crate::tool::ToolError::Denied("required capability is unavailable".into())
                }
                error => {
                    crate::tool::ToolError::Failed(RemoteError::authorization(error).to_string())
                }
            })
    }

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
        // Accepted registrations retain this gate through journal and live
        // publication (including grant/pool invalidation), even if their caller
        // abandons its waiter. Drain them before shutting down remote resources.
        let _mutation = self.mutation.write().await;
        self.remote.shutdown().await;
    }

    pub(crate) async fn add(
        &self,
        mut definition: TargetDefinition,
        origin: String,
        subject: &AuthorizationSubject,
        store: &crate::session::SessionStore,
    ) -> Result<TargetDefinition, RemoteError> {
        definition.origin = origin.clone();
        loop {
            let snapshot = self.targets.definitions().await;
            let resolver: Arc<dyn super::normalize::ConfigResolver> =
                if origin == super::ROOT_TARGET {
                    Arc::new(super::normalize::LocalResolver)
                } else {
                    let route = self.resolve(&origin).await?;
                    self.authorization
                        .authorize(
                            subject,
                            "connect_target".into(),
                            vec![route.permission()],
                            route.authorization_arguments(),
                        )
                        .await
                        .map_err(RemoteError::authorization)?;
                    let workspace = route.destination().workspace.clone();
                    Arc::new(RemoteResolver(
                        self.prepare(route, &workspace, subject).await?,
                    ))
                };
            let normalized = tokio::select! {
                normalized = super::normalize::normalize(vec![definition.clone()], snapshot.clone(), resolver) => normalized?,
                () = subject.cancellation.cancelled() => return Err(RemoteError::Cancelled),
            };
            let mutation = self.mutation.clone().write_owned().await;
            if self.targets.definitions().await != snapshot {
                continue;
            }
            let prepared = self.targets.prepare_upsert_many(normalized).await?;
            let definitions = prepared.definitions();
            // Capture the requested destination from the authoritative staged batch
            // before persistence. Returning it avoids a post-mutation registry lookup.
            let destination = definitions
                .iter()
                .find(|saved| saved.name == definition.name)
                .cloned()
                .ok_or_else(|| {
                    RemoteError::Protocol("normalized target batch omitted destination".into())
                })?;
            if subject.cancellation.is_cancelled() {
                return Err(RemoteError::Cancelled);
            }
            let targets = definitions.to_vec();
            let store = store.clone();
            let agent = subject.agent.clone();
            let router = self.clone();
            // Admission transfers both guards to the owner. Dropping the caller
            // after this point cannot strand a committed journal update without
            // its registry publication and invalidation. Failed/indeterminate
            // appends do not publish definitions or imply a safe retry.
            return tokio::spawn(async move {
                let _mutation = mutation;
                store
                    .append(
                        agent,
                        crate::session::SessionEvent::TargetsUpserted { targets },
                    )
                    .await
                    .map_err(|e| RemoteError::Io {
                        kind: std::io::ErrorKind::Other,
                        message: e.to_string(),
                    })?;
                let (_, invalidated) = prepared.publish().await;
                router
                    .authorization
                    .revoke(|grant| {
                        matches!(&grant.resource, ResourceId::Route { destination, .. }
                            if invalidated.contains(destination))
                    })
                    .await;
                router.remote.invalidate(&invalidated).await;
                Ok(destination)
            })
            .await
            .map_err(|error| {
                RemoteError::Protocol(format!("target publication owner lost: {error}"))
            })?;
        }
    }

    pub fn targets(&self) -> &TargetRegistry {
        &self.targets
    }

    pub async fn resolve(&self, target: &str) -> Result<ResolvedRoute, TargetError> {
        let definitions = self.targets.route(target).await?;
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
            let current = self.resolve(route.identity.destination()).await?;
            if current.identity() == route.identity() && self.remote.is_current(&prepared).await {
                // Route admission linearizes at this successful identity/current-slot
                // comparison under the mutation gate. The prepared connection retains
                // this admitted snapshot; later mutation does not revoke it. This is
                // not an execution-time freshness guarantee, and the gate is not held
                // while establishing the connection or executing remote operations.
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
                    vec![route.permission()],
                    route.authorization_arguments(),
                )
                .await
                .map_err(RemoteError::authorization)?;
        }
    }
}

struct RemoteResolver(PreparedConnection);
#[async_trait::async_trait]
impl super::normalize::ConfigResolver for RemoteResolver {
    async fn resolve(
        &self,
        target: &TargetDefinition,
    ) -> Result<crate::remote::ssh::ResolvedSsh, TargetError> {
        self.0
            .resolve_target(target.clone())
            .await
            .map_err(|e| TargetError::Import(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use futures_util::future::BoxFuture;
    use std::{
        collections::VecDeque,
        path::PathBuf,
        sync::{
            Arc, Mutex as StdMutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use tokio::sync::{Notify, Semaphore};

    use super::*;
    use crate::{
        identity::{AgentId, JobId, SessionId},
        job::CancellationToken,
        remote::{
            ConnectionFactory, ConnectionRequest, EmbeddedShimCatalog, RejectSensitivePrompts,
            test_transport,
        },
        session::{AppendBoundary, SessionStore},
        target::ROOT_TARGET,
        tests::RecordingPolicy,
        tool::policy::PolicyDecision,
    };

    const BOUNDARIES: [AppendBoundary; 4] = [
        AppendBoundary::Write,
        AppendBoundary::Flush,
        AppendBoundary::Sync,
        AppendBoundary::Publication,
    ];

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
                    .filter_map(|permission| permission.proposed_grant.clone())
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
        assert_eq!(route.identity.destination(), "build");
        let hops = vec![("gateway".into(), 7), ("build".into(), 19)];
        assert_eq!(route.identity.hops, hops);
        let resource = ResourceId::route("build", hops);
        assert_eq!(
            route.permission(),
            PermissionUse::new(Capability::Targets, resource.clone())
                .with_grant(ApprovalGrant::exact(Capability::Targets, resource))
        );
        let arguments = route.authorization_arguments();
        assert_eq!(arguments["route"], serde_json::json!(["gateway", "build"]));
    }

    async fn ephemeral_store() -> (tempfile::TempDir, SessionStore) {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::create_ephemeral(directory.path())
            .await
            .unwrap();
        (directory, store)
    }

    fn subject() -> AuthorizationSubject {
        let mut capabilities = crate::tool::policy::CapabilitySet::default();
        capabilities.insert(Capability::Targets);
        AuthorizationSubject {
            agent: AgentId::root(SessionId::from_bytes([7; 16])),
            job: JobId::new(17).unwrap(),
            parent: Some(JobId::new(11).unwrap()),
            scope: Some(23),
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
        router
            .add(definition, ROOT_TARGET.into(), &subject, &store)
            .await
            .unwrap();
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

    struct BlockingFactory {
        starts: AtomicUsize,
        completions: Arc<AtomicUsize>,
        started: Notify,
        release: Arc<Semaphore>,
    }

    impl BlockingFactory {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                starts: AtomicUsize::new(0),
                completions: Arc::new(AtomicUsize::new(0)),
                started: Notify::new(),
                release: Arc::new(Semaphore::new(0)),
            })
        }

        async fn wait_for_starts(&self, expected: usize) {
            while self.starts.load(Ordering::SeqCst) < expected {
                self.started.notified().await;
            }
        }
    }

    impl ConnectionFactory for BlockingFactory {
        fn connect(
            &self,
            request: ConnectionRequest,
        ) -> BoxFuture<'static, Result<crate::remote::backend::Transport, RemoteError>> {
            assert_eq!(request.target, "build");
            assert!(!request.route.is_empty());
            assert_eq!(request.workspace, PathBuf::from("/override"));
            self.starts.fetch_add(1, Ordering::SeqCst);
            self.started.notify_waiters();
            let (release, completions) = (self.release.clone(), self.completions.clone());
            Box::pin(async move {
                release.acquire().await.unwrap().forget();
                completions.fetch_add(1, Ordering::SeqCst);
                Ok(test_transport())
            })
        }
    }

    async fn prepare(
        router: &TargetRouter,
        subject: &AuthorizationSubject,
    ) -> Result<PreparedConnection, RemoteError> {
        let route = router.resolve("build").await.unwrap();
        approve(router, &route, subject).await?;
        router.prepare(route, Path::new("/override"), subject).await
    }

    async fn approve(
        router: &TargetRouter,
        route: &ResolvedRoute,
        subject: &AuthorizationSubject,
    ) -> Result<(), RemoteError> {
        let permissions = vec![route.permission()];
        let arguments = route.authorization_arguments();
        let authorize = router.authorization.authorize(
            subject,
            "connect_target".to_owned(),
            permissions,
            arguments,
        );
        authorize.await.map_err(RemoteError::authorization)
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
        let original = router.resolve("build").await.unwrap();
        assert!(matches!(
            approve(&router, &original, &subject).await,
            Err(RemoteError::ApprovalDenied(reason)) if reason.contains("no")
        ));
        approve(&router, &original, &subject).await.unwrap();

        replace_target(&router, target("gateway", None)).await;
        let changed = router.resolve("build").await.unwrap();
        assert_ne!(original.identity(), changed.identity());
        approve(&router, &changed, &subject).await.unwrap();
        assert_eq!(policy.requests.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn admitted_route_snapshot_survives_later_registry_mutation() {
        let factory = BlockingFactory::new();
        let router = router(
            registry(&[("build", None)]),
            recording([]),
            Some(factory.clone()),
        );
        let admitted = router.resolve("build").await.unwrap();
        factory.release.add_permits(1);
        let prepared = prepare(&router, &subject()).await.unwrap();
        assert!(router.remote.is_current(&prepared).await);

        replace_target(&router, target("build", None)).await;
        let current = router.resolve("build").await.unwrap();
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
        let factory = BlockingFactory::new();
        let router = router(
            registry(&[("build", None)]),
            policy.clone(),
            Some(factory.clone()),
        );
        let subject = subject();
        let route = router.resolve("build").await.unwrap();
        approve(&router, &route, &subject).await.unwrap();
        router.authorization.revoke(|_| true).await;
        factory.release.add_permits(1);
        let prepared = router
            .prepare(route.clone(), Path::new("/override"), &subject)
            .await;
        assert_eq!(policy.requests.lock().unwrap().len(), 1);
        assert!(matches!(approve(&router, &route, &subject).await,
            Err(RemoteError::ApprovalDenied(reason)) if reason == "revoked"));
        drop(prepared.unwrap());
        router.shutdown().await;
    }

    #[tokio::test]
    async fn concurrent_waiters_share_startup_and_cancellation_is_per_waiter() {
        let policy = recording([]);
        let factory = BlockingFactory::new();
        let router = router(
            registry(&[("build", None)]),
            policy.clone(),
            Some(factory.clone()),
        );
        let first_subject = subject();
        let first = spawn_prepare(&router, first_subject.clone());
        factory.wait_for_starts(1).await;
        assert_eq!(policy.requests.lock().unwrap().len(), 1);
        let second = spawn_prepare(&router, subject());
        tokio::task::yield_now().await;
        first_subject.cancellation.cancel();
        assert!(matches!(first.await.unwrap(), Err(RemoteError::Cancelled)));
        factory.release.add_permits(1);
        second.await.unwrap().unwrap();
        assert_eq!(factory.starts.load(Ordering::SeqCst), 1);
        assert_eq!(policy.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn startup_continues_after_its_only_waiter_is_cancelled() {
        let factory = BlockingFactory::new();
        let router = router(
            registry(&[("build", None)]),
            recording([]),
            Some(factory.clone()),
        );
        let first_subject = subject();
        let first = spawn_prepare(&router, first_subject.clone());
        factory.wait_for_starts(1).await;

        first_subject.cancellation.cancel();
        assert!(matches!(first.await.unwrap(), Err(RemoteError::Cancelled)));
        factory.release.add_permits(1);
        for _ in 0..100 {
            if factory.completions.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(factory.completions.load(Ordering::SeqCst), 1);

        prepare(&router, &subject()).await.unwrap();
        assert_eq!(factory.starts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn invalidation_during_startup_retries_before_returning() {
        let policy = recording([]);
        let factory = BlockingFactory::new();
        let targets = registry(&[("gateway", None), ("build", Some("gateway"))]);
        let router = router(targets, policy.clone(), Some(factory.clone()));
        let preparing = spawn_prepare(&router, subject());
        factory.wait_for_starts(1).await;
        replace_target(&router, target("gateway", None)).await;
        factory.release.add_permits(2);
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
                    router
                        .add(definition, ROOT_TARGET.into(), &subject, &store)
                        .await
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
            let saved = targets.get("build").await.unwrap();
            assert_eq!(saved.revision, 2);
            let records = store.records().await;
            assert!(matches!(&records.last().unwrap().event,
                crate::session::SessionEvent::TargetsUpserted { targets } if targets == &[saved]));
            // Router shutdown, like runtime shutdown, does not close the journal.
            assert!(store.drain().await.is_ok());
        }
    }

    #[tokio::test]
    async fn failed_or_indeterminate_target_append_releases_guards_without_publication() {
        for boundary in BOUNDARIES {
            let (_root, store) = ephemeral_store().await;
            let original = target("build", None);
            let targets = registry(&[("build", None)]);
            let router = router(targets.clone(), recording([]), None);
            store.fail_append_at(boundary).await;
            assert!(
                router
                    .add(
                        target("build", None),
                        ROOT_TARGET.into(),
                        &subject_for(&store),
                        &store,
                    )
                    .await
                    .is_err()
            );
            assert_eq!(targets.get("build").await.unwrap(), original);
            assert!(router.mutation.try_write().is_ok());
            let recovery = match store.drain().await {
                Err(crate::session::SessionError::AppendUnavailable(recovery)) => recovery,
                result => panic!("poisoned writer must require recovery on drain: {result:?}"),
            };
            assert_eq!(recovery.identity.session, store.id());
            assert!(!recovery.reason.is_empty());
            router.shutdown().await;
        }
        // A mismatched session rejects append after normalization and graph validation.
        let (_root, store) = ephemeral_store().await;
        let targets = registry(&[("first", None)]);
        let before = targets.definitions().await;
        let router = router(targets.clone(), recording([]), None);
        assert!(
            router
                .add(
                    target("second", None),
                    ROOT_TARGET.into(),
                    &subject(),
                    &store,
                )
                .await
                .is_err()
        );
        assert_eq!(targets.definitions().await, before);
    }
}

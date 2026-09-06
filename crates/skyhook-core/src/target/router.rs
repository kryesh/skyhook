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
    pub destination: String,
    pub hops: Vec<(String, u64)>,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedRoute {
    pub identity: RouteIdentity,
    pub definitions: Vec<TargetDefinition>,
}

impl ResolvedRoute {
    pub fn permission(&self) -> PermissionUse {
        let mut segments = vec![self.identity.destination.clone()];
        segments.extend(
            self.identity
                .hops
                .iter()
                .flat_map(|(name, revision)| [name.clone(), revision.to_string()]),
        );
        let resource = ResourceId::new("route", segments);
        PermissionUse::new(Capability::Targets, resource.clone())
            .with_grant(ApprovalGrant::exact(Capability::Targets, resource))
    }

    pub fn authorization_arguments(&self) -> serde_json::Value {
        serde_json::json!({
            "destination": self.identity.destination,
            "type": self.definitions.last().map(|target| target.r#type),
            "origin": self.definitions.last().map(|target| &target.origin),
            "host": self.definitions.last().map(|target| &target.host),
            "route": self.definitions.iter().map(|hop| &hop.name).collect::<Vec<_>>(),
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
    ) -> Result<crate::remote::authentication::ProcessEnvironment, RemoteError> {
        self.remote.environment().await
    }
    pub(crate) async fn shutdown(&self) {
        self.remote.shutdown().await;
    }

    pub(crate) async fn add(
        &self,
        mut definition: TargetDefinition,
        origin: String,
        subject: &AuthorizationSubject,
        store: &crate::session::SessionStore,
    ) -> Result<Vec<TargetDefinition>, RemoteError> {
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
                    let workspace = route
                        .definitions
                        .last()
                        .ok_or(RemoteError::EmptyRoute)?
                        .workspace
                        .clone();
                    Arc::new(RemoteResolver(
                        self.prepare(route, &workspace, subject).await?,
                    ))
                };
            let normalized = tokio::select! {
                normalized = super::normalize::normalize(vec![definition.clone()], snapshot.clone(), resolver) => normalized?,
                () = subject.cancellation.cancelled() => return Err(RemoteError::Cancelled),
            };
            let _mutation = self.mutation.write().await;
            if self.targets.definitions().await != snapshot {
                continue;
            }
            let staged = TargetRegistry::from_definitions(snapshot)?;
            let (definitions, invalidated) = staged.upsert_many(normalized).await?;
            if subject.cancellation.is_cancelled() {
                return Err(RemoteError::Cancelled);
            }
            store
                .append(
                    subject.agent.clone(),
                    crate::session::SessionEvent::TargetsUpserted {
                        targets: definitions.clone(),
                    },
                )
                .await
                .map_err(|e| RemoteError::Io {
                    kind: std::io::ErrorKind::Other,
                    message: e.to_string(),
                })?;
            self.targets.upsert_many(definitions.clone()).await?;
            self.authorization
                .revoke(|grant| {
                    grant.resource.namespace == "route"
                        && grant
                            .resource
                            .segments
                            .first()
                            .is_some_and(|target| invalidated.contains(target))
                })
                .await;
            self.remote.invalidate(&invalidated).await;
            return Ok(definitions);
        }
    }

    pub fn targets(&self) -> &TargetRegistry {
        &self.targets
    }

    pub async fn resolve(&self, target: &str) -> Result<ResolvedRoute, TargetError> {
        let definitions = self.targets.route(target).await?;
        Ok(ResolvedRoute {
            identity: RouteIdentity {
                destination: target.to_owned(),
                hops: definitions
                    .iter()
                    .map(|hop| (hop.name.clone(), hop.revision))
                    .collect(),
            },
            definitions,
        })
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
            let current = self.resolve(&route.identity.destination).await?;
            if current.identity == route.identity && self.remote.is_current(&prepared).await {
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

    #[cfg(test)]
    pub async fn upsert(
        &self,
        definition: TargetDefinition,
    ) -> Result<TargetDefinition, TargetError> {
        let _mutation = self.mutation.write().await;
        let (definition, invalidated) = self.targets.upsert(definition).await?;
        self.authorization
            .revoke(|grant| {
                grant.resource.namespace == "route"
                    && grant
                        .resource
                        .segments
                        .first()
                        .is_some_and(|target| invalidated.contains(target))
            })
            .await;
        self.remote.invalidate(&invalidated).await;
        Ok(definition)
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
            .resolve_ssh(target.clone())
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
            test_connection,
        },
        tool::{
            authorization::AuthorizationError,
            policy::{AuthorizationRequest, Policy, PolicyDecision, PolicyFuture},
        },
    };

    struct RecordingPolicy {
        decisions: StdMutex<VecDeque<PolicyDecision>>,
        requests: StdMutex<Vec<AuthorizationRequest>>,
    }

    impl RecordingPolicy {
        fn new(decisions: impl IntoIterator<Item = PolicyDecision>) -> Arc<Self> {
            Arc::new(Self {
                decisions: StdMutex::new(decisions.into_iter().collect()),
                requests: StdMutex::new(Vec::new()),
            })
        }
    }

    impl Policy for RecordingPolicy {
        fn authorize(&self, request: AuthorizationRequest) -> PolicyFuture<'_> {
            self.requests.lock().unwrap().push(request);
            let mut decision = self
                .decisions
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(PolicyDecision::allow);
            if let PolicyDecision::Allow { grants } = &mut decision
                && grants.is_empty()
            {
                *grants = self
                    .requests
                    .lock()
                    .unwrap()
                    .last()
                    .unwrap()
                    .permissions
                    .iter()
                    .filter_map(|permission| permission.proposed_grant.clone())
                    .collect();
            }
            Box::pin(async move { decision })
        }
    }

    fn target(name: &str, via: Option<&str>) -> TargetDefinition {
        TargetDefinition::test(name, format!("/{name}"), via)
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

    fn router(
        targets: TargetRegistry,
        policy: Arc<RecordingPolicy>,
        factory: Option<Arc<dyn ConnectionFactory>>,
    ) -> TargetRouter {
        let authorization = AuthorizationCoordinator::new(policy);
        let mut remote = RemoteManager::new(
            EmbeddedShimCatalog::default(),
            Arc::new(RejectSensitivePrompts),
            authorization.clone(),
        );
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
        failure: bool,
    }

    impl BlockingFactory {
        fn new() -> Arc<Self> {
            Self::with_failure(false)
        }

        fn failing() -> Arc<Self> {
            Self::with_failure(true)
        }

        fn with_failure(failure: bool) -> Arc<Self> {
            Arc::new(Self {
                starts: AtomicUsize::new(0),
                completions: Arc::new(AtomicUsize::new(0)),
                started: Notify::new(),
                release: Arc::new(Semaphore::new(0)),
                failure,
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
        ) -> BoxFuture<'static, Result<crate::remote::PooledConnection, RemoteError>> {
            assert_eq!(request.target, "build");
            assert!(!request.route.is_empty());
            assert_eq!(request.workspace, PathBuf::from("/override"));
            self.starts.fetch_add(1, Ordering::SeqCst);
            self.started.notify_waiters();
            let release = self.release.clone();
            let failure = self.failure;
            let completions = self.completions.clone();
            Box::pin(async move {
                release.acquire().await.unwrap().forget();
                completions.fetch_add(1, Ordering::SeqCst);
                if failure {
                    Err(RemoteError::UnsupportedPlatform {
                        arch: "mystery".to_owned(),
                        os: "unknown".to_owned(),
                    })
                } else {
                    Ok(test_connection().await)
                }
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
        router
            .authorization
            .authorize(
                subject,
                "connect_target".to_owned(),
                vec![route.permission()],
                route.authorization_arguments(),
            )
            .await
            .map_err(RemoteError::authorization)
    }

    fn spawn_prepare(
        router: &TargetRouter,
        subject: AuthorizationSubject,
    ) -> tokio::task::JoinHandle<Result<PreparedConnection, RemoteError>> {
        let router = router.clone();
        tokio::spawn(async move { prepare(&router, &subject).await })
    }

    #[tokio::test]
    async fn route_approval_is_identity_scoped_and_has_no_workspace() {
        let targets = TargetRegistry::from_definitions([
            target("gateway", None),
            target("build", Some("gateway")),
        ])
        .unwrap();
        let policy = RecordingPolicy::new([]);
        let router = router(targets, policy.clone(), None);
        let route = router.resolve("build").await.unwrap();
        let subject = subject();

        approve(&router, &route, &subject).await.unwrap();
        approve(&router, &route, &subject).await.unwrap();

        let requests = policy.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.arguments["destination"], "build");
        assert_eq!(request.arguments["route"][0], "gateway");
        assert!(request.arguments.get("workspace").is_none());
    }

    #[tokio::test]
    async fn denial_is_retried_and_jump_updates_change_route_identity() {
        let targets = TargetRegistry::from_definitions([
            target("gateway", None),
            target("build", Some("gateway")),
        ])
        .unwrap();
        let policy = RecordingPolicy::new([
            PolicyDecision::Deny {
                reason: "no".to_owned(),
            },
            PolicyDecision::allow(),
            PolicyDecision::allow(),
        ]);
        let router = router(targets, policy.clone(), None);
        let subject = subject();
        let original = router.resolve("build").await.unwrap();
        assert!(matches!(
            approve(&router, &original, &subject).await,
            Err(RemoteError::ApprovalDenied(reason)) if reason.contains("no")
        ));
        approve(&router, &original, &subject).await.unwrap();

        router.upsert(target("gateway", None)).await.unwrap();
        let changed = router.resolve("build").await.unwrap();
        assert_ne!(original.identity, changed.identity);
        approve(&router, &changed, &subject).await.unwrap();
        assert_eq!(policy.requests.lock().unwrap().len(), 3);
    }

    #[test]
    fn route_authorization_errors_keep_their_categories() {
        assert!(matches!(
            RemoteError::authorization(AuthorizationError::Cancelled),
            RemoteError::Cancelled
        ));
        assert!(matches!(
            RemoteError::authorization(AuthorizationError::InvalidGrant("wide".to_owned())),
            RemoteError::ApprovalInvalidGrant(reason) if reason == "wide"
        ));
        assert!(matches!(
            RemoteError::authorization(AuthorizationError::Unavailable),
            RemoteError::ApprovalUnavailable
        ));
    }

    #[tokio::test]
    async fn concurrent_waiters_share_startup_and_cancellation_is_per_waiter() {
        let targets = TargetRegistry::from_definitions([target("build", None)]).unwrap();
        let policy = RecordingPolicy::new([]);
        let factory = BlockingFactory::new();
        let router = router(targets, policy.clone(), Some(factory.clone()));
        let first_subject = subject();
        let second_subject = subject();
        let first = spawn_prepare(&router, first_subject.clone());
        factory.wait_for_starts(1).await;
        assert_eq!(policy.requests.lock().unwrap().len(), 1);
        let second = spawn_prepare(&router, second_subject);
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
        let targets = TargetRegistry::from_definitions([target("build", None)]).unwrap();
        let policy = RecordingPolicy::new([]);
        let factory = BlockingFactory::new();
        let router = router(targets, policy, Some(factory.clone()));
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
        let targets = TargetRegistry::from_definitions([
            target("gateway", None),
            target("build", Some("gateway")),
        ])
        .unwrap();
        let policy = RecordingPolicy::new([]);
        let factory = BlockingFactory::new();
        let router = router(targets, policy.clone(), Some(factory.clone()));
        let preparing = spawn_prepare(&router, subject());
        factory.wait_for_starts(1).await;
        router.upsert(target("gateway", None)).await.unwrap();
        factory.release.add_permits(2);
        preparing.await.unwrap().unwrap();
        assert_eq!(factory.starts.load(Ordering::SeqCst), 2);
        assert_eq!(policy.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn coalesced_startup_preserves_structured_failure_category() {
        let targets = TargetRegistry::from_definitions([target("build", None)]).unwrap();
        let policy = RecordingPolicy::new([]);
        let factory = BlockingFactory::failing();
        let router = router(targets, policy, Some(factory.clone()));
        let first = spawn_prepare(&router, subject());
        factory.wait_for_starts(1).await;
        let second = spawn_prepare(&router, subject());
        tokio::task::yield_now().await;
        factory.release.add_permits(1);
        for result in [first.await.unwrap(), second.await.unwrap()] {
            assert!(matches!(
                result,
                Err(RemoteError::UnsupportedPlatform { arch, os })
                    if arch == "mystery" && os == "unknown"
            ));
        }
        assert_eq!(factory.starts.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn failed_persistence_does_not_publish_target_registration() {
        let root = tempfile::tempdir().unwrap();
        let store = crate::session::SessionStore::create_ephemeral(root.path())
            .await
            .unwrap();
        let targets = TargetRegistry::from_definitions([target("first", None)]).unwrap();
        let before = targets.definitions().await;
        let router = router(targets.clone(), RecordingPolicy::new([]), None);
        // The mismatched session rejects append after normalization and graph validation.
        assert!(
            router
                .add(
                    target("second", None),
                    crate::target::ROOT_TARGET.into(),
                    &subject(),
                    &store
                )
                .await
                .is_err()
        );
        assert_eq!(targets.definitions().await, before);
    }
}

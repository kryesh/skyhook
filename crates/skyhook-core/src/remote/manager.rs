use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use futures_util::future::{BoxFuture, FutureExt as _, Shared};
use thiserror::Error;
use tokio::sync::Mutex;

use crate::{
    job::CancellationToken,
    remote::{ArtifactError, EmbeddedShimCatalog, SensitivePromptHandler},
    target::{ResolvedRoute, RouteIdentity, TargetDefinition, TargetError},
    tool::{
        ToolContext, ToolError, ToolOutput,
        authorization::{AuthorizationCoordinator, AuthorizationError},
    },
};

pub(crate) use super::client::PooledConnection;
pub(super) use super::client::Session;

#[derive(Clone)]
pub(crate) struct RemoteManager {
    inner: Arc<RemoteInner>,
}

struct RemoteInner {
    factory: Arc<dyn super::backend::ConnectionFactory>,
    shutdown: CancellationToken,
    pool: Mutex<HashMap<ConnectionKey, Arc<PooledSlot>>>,
    prompts: Arc<dyn SensitivePromptHandler>,
    authorization: AuthorizationCoordinator,
}

struct PooledSlot {
    connection: Shared<BoxFuture<'static, Result<Session, RemoteError>>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ConnectionKey {
    route: RouteIdentity,
    workspace: PathBuf,
}

#[derive(Clone)]
pub(crate) struct PreparedConnection {
    manager: RemoteManager,
    key: ConnectionKey,
    slot: Arc<PooledSlot>,
    connection: Session,
}

impl PreparedConnection {
    pub(crate) async fn execute(
        self,
        name: String,
        arguments: serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolOutput, RemoteError> {
        let result = self.connection.execute(name, arguments, context).await;
        if matches!(
            result,
            Err(RemoteError::Io { .. } | RemoteError::Protocol(_))
        ) {
            self.manager.discard(&self).await;
        }
        result
    }
}

impl RemoteManager {
    pub(crate) fn new(
        catalog: EmbeddedShimCatalog,
        prompts: Arc<dyn SensitivePromptHandler>,
        authorization: AuthorizationCoordinator,
    ) -> Self {
        Self {
            inner: Arc::new(RemoteInner {
                factory: Arc::new(super::backend::Backends::new(catalog, prompts.clone())),
                shutdown: CancellationToken::new(),
                pool: Mutex::new(HashMap::new()),
                prompts,
                authorization,
            }),
        }
    }

    pub(crate) async fn environment(
        &self,
    ) -> Result<super::backend::ProcessEnvironment, RemoteError> {
        if self.inner.shutdown.is_cancelled() {
            return Err(RemoteError::Cancelled);
        }
        self.inner.factory.environment().await
    }
    pub(crate) async fn shutdown(&self) {
        self.inner.shutdown.cancel();
        self.inner.pool.lock().await.clear();
        self.inner.factory.shutdown().await;
    }

    #[cfg(test)]
    pub(crate) fn with_connection_factory(
        mut self,
        factory: Arc<dyn super::backend::ConnectionFactory>,
    ) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("connection factories are installed before sharing a manager")
            .factory = factory;
        self
    }

    pub(crate) fn connection<'a>(
        &'a self,
        route: ResolvedRoute,
        workspace: &'a Path,
        cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<PreparedConnection, RemoteError>> {
        Box::pin(async move {
            let key = ConnectionKey {
                route: route.identity.clone(),
                workspace: workspace.to_path_buf(),
            };
            let slot = {
                let mut pool = self.inner.pool.lock().await;
                if let Some(existing) = pool.get(&key) {
                    existing.clone()
                } else {
                    let manager = self.clone();
                    let target = route.identity.destination.clone();
                    let definitions = route.definitions;
                    let workspace = workspace.to_path_buf();
                    let startup = tokio::spawn(async move {
                        let connect =
                            async { manager.connect(&target, &definitions, &workspace).await };
                        tokio::select! { result = connect => result, () = manager.inner.shutdown.cancelled() => Err(RemoteError::Cancelled) }
                    });
                    let connection = async move {
                        startup
                            .await
                            .map_err(|error| RemoteError::ConnectionTask(error.to_string()))?
                    }
                    .boxed()
                    .shared();
                    let slot = Arc::new(PooledSlot { connection });
                    pool.insert(key.clone(), slot.clone());
                    slot
                }
            };
            let result = tokio::select! {
                result = slot.connection.clone() => result,
                () = cancellation.cancelled() => return Err(RemoteError::Cancelled),
            };
            match result {
                Ok(connection) => Ok(PreparedConnection {
                    manager: self.clone(),
                    key,
                    slot,
                    connection,
                }),
                Err(error) => {
                    self.remove_slot(&key, &slot).await;
                    Err(error)
                }
            }
        })
    }

    pub(crate) async fn is_current(&self, prepared: &PreparedConnection) -> bool {
        self.inner
            .pool
            .lock()
            .await
            .get(&prepared.key)
            .is_some_and(|slot| Arc::ptr_eq(slot, &prepared.slot))
    }

    pub(crate) async fn discard(&self, prepared: &PreparedConnection) {
        self.remove_slot(&prepared.key, &prepared.slot).await;
    }

    async fn remove_slot(&self, key: &ConnectionKey, slot: &Arc<PooledSlot>) {
        let mut pool = self.inner.pool.lock().await;
        if pool
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, slot))
        {
            pool.remove(key);
        }
    }

    pub(crate) async fn invalidate(&self, names: &[String]) {
        self.inner
            .pool
            .lock()
            .await
            .retain(|key, _| !names.contains(&key.route.destination));
    }

    async fn connect(
        &self,
        target: &str,
        route: &[TargetDefinition],
        workspace: &Path,
    ) -> Result<Session, RemoteError> {
        let destination = route.last().ok_or(RemoteError::EmptyRoute)?;
        let (origin, hops) = if destination.origin == crate::target::ROOT_TARGET {
            (None, route)
        } else {
            let index = route
                .iter()
                .position(|hop| hop.name == destination.origin)
                .ok_or_else(|| RemoteError::Protocol("origin missing from route".into()))?;
            let definitions = route[..=index].to_vec();
            let identity = RouteIdentity {
                destination: destination.origin.clone(),
                hops: definitions
                    .iter()
                    .map(|h| (h.name.clone(), h.revision))
                    .collect(),
            };
            let cancellation = CancellationToken::new();
            let prepared = self
                .connection(
                    ResolvedRoute {
                        identity,
                        definitions,
                    },
                    &route[index].workspace,
                    &cancellation,
                )
                .await?;
            (Some(prepared.connection), &route[index + 1..])
        };
        let mut transport = self
            .inner
            .factory
            .connect(super::backend::ConnectionRequest {
                target: target.to_owned(),
                route: hops.to_vec(),
                workspace: workspace.to_path_buf(),
                origin: origin.clone(),
            })
            .await?;
        // Retain the credential-owning session for the whole child stream,
        // independently of the backend's own transport lifetime guards.
        transport.owner = Box::new((transport.owner, origin));
        Ok(Arc::new(
            PooledConnection::from_transport(
                transport,
                target,
                self.inner.authorization.clone(),
                self.inner.prompts.clone(),
            )
            .await?,
        ))
    }
}

#[derive(Clone, Debug, Error)]
pub enum RemoteError {
    #[error(transparent)]
    Target(#[from] TargetError),
    #[error(transparent)]
    Artifact(#[from] ArtifactError),
    #[error(
        "this Skyhook build contains no remote shims; install with default features or provide an EmbeddedShimCatalog"
    )]
    MissingShims,
    #[error(
        "unsupported remote platform {os}-{protocol}-{arch}: no matching {protocol} shim is embedded"
    )]
    UnsupportedPlatform {
        protocol: String,
        arch: String,
        os: String,
    },
    #[error("could not start transport process: {message}")]
    Start {
        kind: std::io::ErrorKind,
        message: String,
    },
    #[error("SSH configuration resolution failed: {0}")]
    Resolution(String),
    #[error("target connection was denied: {0}")]
    ApprovalDenied(String),
    #[error("target connection returned an invalid approval grant: {0}")]
    ApprovalInvalidGrant(String),
    #[error("target connection requires an unavailable capability")]
    ApprovalUnavailable,
    #[error("target connection was cancelled")]
    Cancelled,
    #[error("remote connection startup task failed: {0}")]
    ConnectionTask(String),
    #[error("SSH failed: {0}")]
    Ssh(String),
    #[error("remote shim deployment failed: {0}")]
    Deployment(String),
    #[error("remote protocol failed: {0}")]
    Protocol(String),
    #[error("remote operation denied: {0}")]
    OperationDenied(String),
    #[error("remote tool failed: {message}")]
    Remote {
        message: String,
        output: Option<Box<ToolOutput>>,
    },
    #[error("remote connection is missing {0}")]
    MissingPipe(&'static str),
    #[error("target route is empty")]
    EmptyRoute,
    #[error("remote platform probe returned invalid output")]
    InvalidProbe,
    #[error("SSH values cannot be empty or contain control characters")]
    InvalidSshValue,
    #[error("{message}")]
    Io {
        kind: std::io::ErrorKind,
        message: String,
    },
    #[error("{0}")]
    Json(String),
}

impl RemoteError {
    pub(crate) fn authorization(error: AuthorizationError) -> Self {
        match error {
            AuthorizationError::Denied(reason) => Self::ApprovalDenied(reason),
            AuthorizationError::Cancelled => Self::Cancelled,
            AuthorizationError::InvalidGrant(reason) => Self::ApprovalInvalidGrant(reason),
            AuthorizationError::Unavailable => Self::ApprovalUnavailable,
        }
    }

    pub(crate) fn start(error: std::io::Error) -> Self {
        Self::Start {
            kind: error.kind(),
            message: error.to_string(),
        }
    }

    pub(super) fn io(error: std::io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            message: error.to_string(),
        }
    }

    #[must_use]
    pub fn into_tool_error(self) -> ToolError {
        match self {
            Self::OperationDenied(reason) => ToolError::Denied(reason),
            Self::Remote {
                message,
                output: Some(output),
            } => ToolError::with_output(message, *output),
            Self::Remote {
                message,
                output: None,
            } => ToolError::Failed(message),
            error => ToolError::Failed(error.to_string()),
        }
    }
}

impl From<std::io::Error> for RemoteError {
    fn from(error: std::io::Error) -> Self {
        Self::io(error)
    }
}

impl From<serde_json::Error> for RemoteError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error.to_string())
    }
}

impl PreparedConnection {
    pub(crate) async fn resolve_target(
        &self,
        target: TargetDefinition,
    ) -> Result<super::ssh::ResolvedSsh, RemoteError> {
        let result = super::backend::resolve_on(&self.connection, target).await;
        if matches!(
            result,
            Err(RemoteError::Io { .. } | RemoteError::Protocol(_))
        ) {
            self.manager.discard(self).await;
        }
        result
    }
}

#[cfg(test)]
#[path = "integration.rs"]
mod integration;

#[cfg(test)]
mod factory_tests {
    use super::*;
    use crate::remote::{
        RejectSensitivePrompts,
        backend::{ConnectionFactory, ConnectionRequest, Transport},
        client::test_transport,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Debug, PartialEq, Eq)]
    struct RecordedRequest {
        target: String,
        route: Vec<String>,
        has_origin: bool,
        workspace: PathBuf,
    }

    #[derive(Default)]
    struct RecordingFactory {
        requests: std::sync::Mutex<Vec<RecordedRequest>>,
        invalid: AtomicBool,
        rejected_owners: Arc<AtomicUsize>,
        origins: std::sync::Mutex<Vec<std::sync::Weak<PooledConnection>>>,
    }

    struct RejectedOwner(Arc<AtomicUsize>);
    impl Drop for RejectedOwner {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl ConnectionFactory for RecordingFactory {
        fn connect(
            &self,
            request: ConnectionRequest,
        ) -> BoxFuture<'_, Result<Transport, RemoteError>> {
            if let Some(origin) = &request.origin {
                self.origins.lock().unwrap().push(Arc::downgrade(origin));
            }
            self.requests.lock().unwrap().push(RecordedRequest {
                target: request.target,
                route: request.route.iter().map(|hop| hop.name.clone()).collect(),
                has_origin: request.origin.is_some(),
                workspace: request.workspace,
            });
            Box::pin(async move {
                if self.invalid.load(Ordering::SeqCst) {
                    Ok(Transport {
                        input: Box::new(tokio::io::sink()),
                        output: Box::new(tokio::io::empty()),
                        owner: Box::new(RejectedOwner(self.rejected_owners.clone())),
                    })
                } else {
                    Ok(test_transport())
                }
            })
        }
    }

    fn manager(factory: Arc<dyn ConnectionFactory>) -> RemoteManager {
        RemoteManager::new(
            EmbeddedShimCatalog::default(),
            Arc::new(RejectSensitivePrompts),
            AuthorizationCoordinator::new(Arc::new(crate::tool::policy::AllowAll)),
        )
        .with_connection_factory(factory)
    }

    struct PendingHandshakeFactory {
        starts: AtomicUsize,
        hello: Arc<tokio::sync::Semaphore>,
        ready: Arc<tokio::sync::Semaphore>,
        dropped: Arc<AtomicUsize>,
    }

    impl PendingHandshakeFactory {
        fn new() -> Self {
            Self {
                starts: AtomicUsize::new(0),
                hello: Arc::new(tokio::sync::Semaphore::new(0)),
                ready: Arc::new(tokio::sync::Semaphore::new(0)),
                dropped: Arc::new(AtomicUsize::new(0)),
            }
        }

        async fn wait_for_hello(&self) {
            tokio::time::timeout(std::time::Duration::from_secs(5), self.hello.acquire())
                .await
                .expect("common handshake did not start")
                .unwrap()
                .forget();
        }
    }

    struct PendingHandshakeOwner {
        shim: tokio::task::JoinHandle<()>,
        dropped: Arc<AtomicUsize>,
    }

    impl Drop for PendingHandshakeOwner {
        fn drop(&mut self) {
            self.shim.abort();
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl ConnectionFactory for PendingHandshakeFactory {
        fn connect(&self, _: ConnectionRequest) -> BoxFuture<'_, Result<Transport, RemoteError>> {
            Box::pin(async move {
                use crate::remote::protocol::{Request, Response, read_frame, write_frame};

                self.starts.fetch_add(1, Ordering::SeqCst);
                let hello = self.hello.clone();
                let ready = self.ready.clone();
                let (client, mut shim) = tokio::io::duplex(4096);
                let shim = tokio::spawn(async move {
                    if !matches!(
                        read_frame::<_, Request>(&mut shim).await,
                        Ok(Some(Request::Hello { .. }))
                    ) {
                        return;
                    }
                    hello.add_permits(1);
                    ready.acquire().await.unwrap().forget();
                    if write_frame(
                        &mut shim,
                        &Response::Ready {
                            version: super::super::protocol::PROTOCOL_VERSION,
                        },
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                    while let Ok(Some(_)) = read_frame::<_, Request>(&mut shim).await {}
                });
                let (output, input) = tokio::io::split(client);
                Ok(Transport {
                    input: Box::new(input),
                    output: Box::new(output),
                    owner: Box::new(PendingHandshakeOwner {
                        shim,
                        dropped: self.dropped.clone(),
                    }),
                })
            })
        }
    }

    fn route(definitions: Vec<TargetDefinition>) -> ResolvedRoute {
        ResolvedRoute {
            identity: RouteIdentity {
                destination: definitions.last().unwrap().name.clone(),
                hops: definitions
                    .iter()
                    .map(|hop| (hop.name.clone(), hop.revision))
                    .collect(),
            },
            definitions,
        }
    }

    #[tokio::test]
    async fn every_factory_uses_common_handshake_and_failed_slots_are_retryable() {
        let factory = Arc::new(RecordingFactory::default());
        factory.invalid.store(true, Ordering::SeqCst);
        let manager = manager(factory.clone());
        let route = route(vec![TargetDefinition::test("build", "/build", None)]);
        let cancellation = CancellationToken::new();
        let failure = manager
            .connection(route.clone(), Path::new("/build"), &cancellation)
            .await;
        assert!(
            matches!(failure, Err(RemoteError::Protocol(message)) if message == "invalid shim handshake")
        );
        assert_eq!(factory.rejected_owners.load(Ordering::SeqCst), 1);
        assert!(manager.inner.pool.lock().await.is_empty());
        factory.invalid.store(false, Ordering::SeqCst);
        let first = manager
            .connection(route.clone(), Path::new("/build"), &cancellation)
            .await
            .unwrap();
        let second = manager
            .connection(route, Path::new("/build"), &cancellation)
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&first.connection, &second.connection));
        assert_eq!(factory.requests.lock().unwrap().len(), 2);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn cancelling_one_waiter_preserves_shared_pending_handshake() {
        let factory = Arc::new(PendingHandshakeFactory::new());
        let manager = manager(factory.clone());
        let route = route(vec![TargetDefinition::test("build", "/build", None)]);
        let cancelled = CancellationToken::new();
        let survivor = CancellationToken::new();
        let first = manager.connection(route.clone(), Path::new("/build"), &cancelled);
        let second = manager.connection(route, Path::new("/build"), &survivor);
        tokio::pin!(first, second);
        assert!(futures_util::poll!(&mut first).is_pending());
        factory.wait_for_hello().await;
        assert!(futures_util::poll!(&mut second).is_pending());

        cancelled.cancel();
        assert!(matches!(first.await, Err(RemoteError::Cancelled)));
        assert_eq!(factory.starts.load(Ordering::SeqCst), 1);
        assert_eq!(factory.dropped.load(Ordering::SeqCst), 0);
        assert!(futures_util::poll!(&mut second).is_pending());

        factory.ready.add_permits(1);
        let prepared = tokio::time::timeout(std::time::Duration::from_secs(5), second)
            .await
            .expect("surviving waiter did not finish the common handshake")
            .unwrap();
        assert_eq!(factory.starts.load(Ordering::SeqCst), 1);
        assert_eq!(factory.dropped.load(Ordering::SeqCst), 0);
        drop(prepared);
        manager.shutdown().await;
        assert_eq!(factory.dropped.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn shutdown_drops_transport_during_shared_pending_handshake() {
        let factory = Arc::new(PendingHandshakeFactory::new());
        let manager = manager(factory.clone());
        let route = route(vec![TargetDefinition::test("build", "/build", None)]);
        let cancellation = CancellationToken::new();
        let first = manager.connection(route.clone(), Path::new("/build"), &cancellation);
        let second = manager.connection(route, Path::new("/build"), &cancellation);
        tokio::pin!(first, second);
        assert!(futures_util::poll!(&mut first).is_pending());
        factory.wait_for_hello().await;
        assert!(futures_util::poll!(&mut second).is_pending());
        assert_eq!(factory.starts.load(Ordering::SeqCst), 1);
        assert_eq!(factory.dropped.load(Ordering::SeqCst), 0);

        manager.shutdown().await;
        let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(first, second)
        })
        .await
        .expect("shutdown did not cancel the common handshake");
        assert!(matches!(first, Err(RemoteError::Cancelled)));
        assert!(matches!(second, Err(RemoteError::Cancelled)));
        assert_eq!(factory.dropped.load(Ordering::SeqCst), 1);
        assert!(manager.inner.pool.lock().await.is_empty());
    }

    #[tokio::test]
    async fn factory_receives_only_route_after_credential_origin() {
        let factory = Arc::new(RecordingFactory::default());
        let manager = manager(factory.clone());
        let origin = TargetDefinition::test("origin", "/origin", None);
        let mut via = TargetDefinition::test("via", "/via", Some("origin"));
        via.origin = "origin".into();
        let mut destination = TargetDefinition::test("build", "/build", Some("via"));
        destination.origin = "origin".into();
        let prepared = manager
            .connection(
                route(vec![origin, via, destination]),
                Path::new("/override"),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            *factory.requests.lock().unwrap(),
            vec![
                RecordedRequest {
                    target: "origin".into(),
                    route: vec!["origin".into()],
                    has_origin: false,
                    workspace: "/origin".into(),
                },
                RecordedRequest {
                    target: "build".into(),
                    route: vec!["via".into(), "build".into()],
                    has_origin: true,
                    workspace: "/override".into(),
                },
            ]
        );
        let origin = factory.origins.lock().unwrap()[0].clone();
        manager.invalidate(&["origin".into()]).await;
        assert!(
            origin.upgrade().is_some(),
            "child stream must retain its origin"
        );
        drop(prepared);
        manager.shutdown().await;
        assert!(
            origin.upgrade().is_none(),
            "shutdown must release transport owners"
        );
    }
}

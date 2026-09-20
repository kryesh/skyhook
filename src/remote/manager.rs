use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use futures_util::future::{BoxFuture, FutureExt as _, Shared};
use tokio::sync::Mutex;

use crate::{
    job::CancellationToken,
    remote::{EmbeddedShimCatalog, SensitivePromptHandler},
    target::{ResolvedRoute, RouteIdentity},
    tool::{ToolContext, ToolOutput, authorization::AuthorizationCoordinator},
};

pub(crate) use super::client::PooledConnection;
pub(super) use super::client::Session;
pub use super::error::RemoteError;

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
                factory: Arc::new(super::ssh::Backend::new(catalog, prompts.clone())),
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
                route: route.identity().clone(),
                workspace: workspace.to_path_buf(),
            };
            let slot = {
                let mut pool = self.inner.pool.lock().await;
                if let Some(existing) = pool.get(&key) {
                    existing.clone()
                } else {
                    let manager = self.clone();
                    let workspace = workspace.to_path_buf();
                    let startup = tokio::spawn(async move {
                        let connect = async { manager.connect(&route, &workspace).await };
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
            .retain(|key, _| !names.iter().any(|name| name == key.route.destination()));
    }

    async fn connect(
        &self,
        resolved_route: &ResolvedRoute,
        workspace: &Path,
    ) -> Result<Session, RemoteError> {
        let target = resolved_route.destination().name.as_str();
        let route = resolved_route.definitions();
        // The destination's origin starts this connection's SSH process on its shim;
        // the hops after it are native jumps of that process.
        let (origin, hops) = match &resolved_route.destination().origin {
            None => (None, route),
            Some(origin) => {
                let index = route
                    .iter()
                    .position(|hop| &hop.name == origin)
                    .ok_or_else(|| RemoteError::Protocol("origin missing from route".into()))?;
                let prefix = ResolvedRoute::from_definitions(route[..=index].to_vec())
                    .expect("inclusive route prefix is nonempty");
                let cancellation = CancellationToken::new();
                let prepared = self
                    .connection(prefix, &route[index].workspace, &cancellation)
                    .await?;
                (Some(prepared.connection), &route[index + 1..])
            }
        };
        let mut transport = self
            .inner
            .factory
            .connect(super::backend::ConnectionRequest {
                route: hops.to_vec(),
                workspace: workspace.to_path_buf(),
                origin: origin.clone(),
            })
            .await?;
        // Retain the origin's session for the whole child stream, independently
        // of the backend's own transport lifetime guards.
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::remote::{
        RejectSensitivePrompts,
        backend::{ConnectionFactory, ConnectionRequest, Transport},
        client::test_transport,
        protocol::{Request, Response, read_frame, write_frame},
    };
    use crate::{target::TargetDefinition, tool::policy::AllowAll};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::Semaphore;

    const FIVE_SECONDS: std::time::Duration = std::time::Duration::from_secs(5);

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

    /// Counts transport-owner drops, aborting any fake shim task it owns.
    struct DropCounter(Arc<AtomicUsize>, Option<tokio::task::JoinHandle<()>>);

    impl Drop for DropCounter {
        fn drop(&mut self) {
            if let Some(shim) = &self.1 {
                shim.abort();
            }
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
                target: request.route.last().unwrap().name.clone(),
                route: request.route.iter().map(|hop| hop.name.clone()).collect(),
                has_origin: request.origin.is_some(),
                workspace: request.workspace,
            });
            Box::pin(async move {
                if !self.invalid.load(Ordering::SeqCst) {
                    return Ok(test_transport());
                }
                Ok(Transport {
                    input: Box::new(tokio::io::sink()),
                    output: Box::new(tokio::io::empty()),
                    owner: Box::new(DropCounter(self.rejected_owners.clone(), None)),
                })
            })
        }
    }

    fn allow_all() -> AuthorizationCoordinator {
        AuthorizationCoordinator::new(Arc::new(AllowAll))
    }

    fn manager(factory: Arc<dyn ConnectionFactory>) -> RemoteManager {
        let prompts = Arc::new(RejectSensitivePrompts);
        RemoteManager::new(EmbeddedShimCatalog::default(), prompts, allow_all())
            .with_connection_factory(factory)
    }

    /// Counts transports it starts and holds each shim handshake until `ready` has a permit.
    pub(crate) struct PendingHandshakeFactory {
        pub starts: AtomicUsize,
        hello: Arc<Semaphore>,
        pub ready: Arc<Semaphore>,
        dropped: Arc<AtomicUsize>,
    }

    impl PendingHandshakeFactory {
        pub fn new() -> Arc<Self> {
            Arc::new(Self {
                starts: AtomicUsize::new(0),
                hello: Arc::new(Semaphore::new(0)),
                ready: Arc::new(Semaphore::new(0)),
                dropped: Arc::new(AtomicUsize::new(0)),
            })
        }

        pub async fn wait_for_hello(&self) {
            let permit = tokio::time::timeout(FIVE_SECONDS, self.hello.acquire()).await;
            permit
                .expect("common handshake did not start")
                .unwrap()
                .forget();
        }

        /// (transports started, transport owners dropped)
        fn counts(&self) -> (usize, usize) {
            let starts = self.starts.load(Ordering::SeqCst);
            (starts, self.dropped.load(Ordering::SeqCst))
        }
    }

    impl ConnectionFactory for PendingHandshakeFactory {
        fn connect(&self, _: ConnectionRequest) -> BoxFuture<'_, Result<Transport, RemoteError>> {
            Box::pin(async move {
                self.starts.fetch_add(1, Ordering::SeqCst);
                let (hello, ready) = (self.hello.clone(), self.ready.clone());
                let (client, mut shim) = tokio::io::duplex(4096);
                let shim = tokio::spawn(async move {
                    let request = read_frame::<_, Request>(&mut shim).await;
                    if !matches!(request, Ok(Some(Request::Hello))) {
                        return;
                    }
                    hello.add_permits(1);
                    ready.acquire().await.unwrap().forget();
                    if write_frame(&mut shim, &Response::Ready).await.is_ok() {
                        while let Ok(Some(_)) = read_frame::<_, Request>(&mut shim).await {}
                    }
                });
                let (output, input) = tokio::io::split(client);
                Ok(Transport {
                    input: Box::new(input),
                    output: Box::new(output),
                    owner: Box::new(DropCounter(self.dropped.clone(), Some(shim))),
                })
            })
        }
    }

    fn route(definitions: Vec<TargetDefinition>) -> ResolvedRoute {
        ResolvedRoute::from_definitions(definitions).expect("test route is nonempty")
    }

    fn build_route() -> ResolvedRoute {
        route(vec![TargetDefinition::test("build", "/build", None)])
    }

    #[tokio::test]
    async fn every_factory_uses_common_handshake_and_failed_slots_are_retryable() {
        let factory = Arc::new(RecordingFactory::default());
        factory.invalid.store(true, Ordering::SeqCst);
        let manager = manager(factory.clone());
        let cancellation = CancellationToken::new();
        let connect = || manager.connection(build_route(), Path::new("/build"), &cancellation);
        assert!(
            matches!(connect().await, Err(RemoteError::Protocol(message)) if message == "invalid shim handshake")
        );
        assert_eq!(factory.rejected_owners.load(Ordering::SeqCst), 1);
        assert!(manager.inner.pool.lock().await.is_empty());
        factory.invalid.store(false, Ordering::SeqCst);
        let first = connect().await.unwrap();
        let second = connect().await.unwrap();
        assert!(Arc::ptr_eq(&first.connection, &second.connection));
        assert_eq!(factory.requests.lock().unwrap().len(), 2);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn startup_outlives_a_cancelled_waiter_and_is_shared_with_pending_and_later_waiters() {
        let factory = PendingHandshakeFactory::new();
        let manager = manager(factory.clone());
        let (cancelled, survivor) = (CancellationToken::new(), CancellationToken::new());
        let first = manager.connection(build_route(), Path::new("/build"), &cancelled);
        let joined = manager.connection(build_route(), Path::new("/build"), &survivor);
        tokio::pin!(first, joined);
        assert!(futures_util::poll!(&mut first).is_pending());
        factory.wait_for_hello().await;
        assert!(futures_util::poll!(&mut joined).is_pending());
        cancelled.cancel();
        assert!(matches!(first.await, Err(RemoteError::Cancelled)));
        assert!(futures_util::poll!(&mut joined).is_pending());
        assert_eq!(factory.counts(), (1, 0));

        // A waiter arriving after the cancellation joins the same startup too.
        let second = manager.connection(build_route(), Path::new("/build"), &survivor);
        tokio::pin!(second);
        assert!(futures_util::poll!(&mut second).is_pending());
        factory.ready.add_permits(1);
        tokio::time::timeout(FIVE_SECONDS, joined)
            .await
            .expect("pending waiter did not finish the common handshake")
            .unwrap();
        let prepared = tokio::time::timeout(FIVE_SECONDS, second)
            .await
            .expect("surviving waiter did not finish the common handshake")
            .unwrap();
        assert_eq!(factory.counts(), (1, 0));
        drop(prepared);
        manager.shutdown().await;
        assert_eq!(factory.counts(), (1, 1));
    }

    #[tokio::test]
    async fn shutdown_drops_transport_during_shared_pending_handshake() {
        let factory = PendingHandshakeFactory::new();
        let manager = manager(factory.clone());
        let cancellation = CancellationToken::new();
        let first = manager.connection(build_route(), Path::new("/build"), &cancellation);
        let second = manager.connection(build_route(), Path::new("/build"), &cancellation);
        tokio::pin!(first, second);
        assert!(futures_util::poll!(&mut first).is_pending());
        factory.wait_for_hello().await;
        assert!(futures_util::poll!(&mut second).is_pending());
        assert_eq!(factory.counts(), (1, 0));

        manager.shutdown().await;
        let (first, second) =
            tokio::time::timeout(FIVE_SECONDS, async { tokio::join!(first, second) })
                .await
                .expect("shutdown did not cancel the common handshake");
        assert!(matches!(first, Err(RemoteError::Cancelled)));
        assert!(matches!(second, Err(RemoteError::Cancelled)));
        assert_eq!(factory.counts(), (1, 1));
        assert!(manager.inner.pool.lock().await.is_empty());
    }

    #[tokio::test]
    async fn factory_receives_only_route_after_ssh_origin() {
        let factory = Arc::new(RecordingFactory::default());
        let manager = manager(factory.clone());
        let origin = TargetDefinition::test("origin", "/origin", None);
        let mut via = TargetDefinition::test("via", "/via", None);
        via.origin = Some("origin".into());
        // The jump belongs to the SSH process the destination's origin starts.
        let mut destination = TargetDefinition::test("build", "/build", Some("via"));
        destination.origin = Some("origin".into());
        // Origins nest: deep's SSH process runs on build's shim.
        let mut deep = TargetDefinition::test("deep", "/deep", None);
        deep.origin = Some("build".into());
        let route = route(vec![origin, via, destination, deep]);
        let cancellation = CancellationToken::new();
        let prepared = manager.connection(route, Path::new("/override"), &cancellation);
        let prepared = prepared.await.unwrap();
        let expected = [
            ("origin", vec!["origin"], false, "/origin"),
            ("build", vec!["via", "build"], true, "/build"),
            ("deep", vec!["deep"], true, "/override"),
        ]
        .map(|(target, route, has_origin, workspace)| RecordedRequest {
            target: target.into(),
            route: route.into_iter().map(String::from).collect(),
            has_origin,
            workspace: workspace.into(),
        });
        assert_eq!(*factory.requests.lock().unwrap(), expected);
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

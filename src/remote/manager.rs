use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
};

use futures_util::future::{BoxFuture, FutureExt as _, Shared};

use super::{
    artifact::EmbeddedShimCatalog,
    backend::{ConnectionFactory, ConnectionRequest, ProcessAuthentication, ProcessEnvironment},
    client::{PooledConnection, Session},
    error::{ProtocolError, RemoteError},
    prompt::SensitivePromptHandler,
};
use crate::{
    execution::ExecutionLocation,
    job::CancellationToken,
    target::{ResolvedRoute, Route, RouteIdentity, TargetDefinition},
    tool::authorization::AuthorizationError,
    tool::{ToolContext, ToolOutput},
};

#[derive(Clone)]
pub(crate) struct RemoteManager {
    inner: Arc<RemoteInner>,
}

struct RemoteInner {
    factory: Arc<dyn ConnectionFactory>,
    shutdown: CancellationToken,
    tasks: tokio_util::task::TaskTracker,
    pool: Mutex<HashMap<ConnectionKey, Arc<PooledSlot>>>,
    prompts: Arc<dyn SensitivePromptHandler>,
}

struct PooledSlot {
    connection: Shared<BoxFuture<'static, Result<Session, RemoteError>>>,
    /// Stops the startup, and any prompt it shows, once a redefinition replaces
    /// a target on its route.
    startup: CancellationToken,
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
        self.release().await;
        result
    }

    /// Read a source file for `tool` on the connected machine into a local spool.
    pub(crate) async fn read_source(
        self,
        tool: String,
        path: String,
        context: &ToolContext,
    ) -> Result<crate::tool::source::Source, RemoteError> {
        let result = self.connection.read_source(tool, path, context).await;
        self.release().await;
        result
    }

    /// Worker I/O failures do not imply that the pooled transport failed.
    async fn release(&self) {
        if self.connection.is_failed().await {
            self.manager.discard(self);
        }
    }
}

impl RemoteManager {
    /// A manager whose startups and connections stop once `shutdown` is cancelled,
    /// as the session's own token is when its journal fails. Local askpass prompts
    /// name the running job of `jobs` they came from.
    pub(crate) fn new(
        catalog: EmbeddedShimCatalog,
        prompts: Arc<dyn SensitivePromptHandler>,
        jobs: Option<crate::job::JobManager>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            inner: Arc::new(RemoteInner {
                factory: Arc::new(super::ssh::Backend::new(catalog, prompts.clone(), jobs)),
                shutdown,
                tasks: tokio_util::task::TaskTracker::new(),
                pool: Mutex::new(HashMap::new()),
                prompts,
            }),
        }
    }

    pub(crate) async fn environment(
        &self,
        authentication: ProcessAuthentication,
    ) -> Result<ProcessEnvironment, RemoteError> {
        if self.inner.shutdown.is_cancelled() {
            return Err(RemoteError::Cancelled);
        }
        self.inner.factory.environment(authentication).await
    }
    fn pool(&self) -> std::sync::MutexGuard<'_, HashMap<ConnectionKey, Arc<PooledSlot>>> {
        self.inner
            .pool
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) async fn shutdown(&self) {
        self.inner.shutdown.cancel();
        {
            let mut pool = self.pool();
            pool.clear();
            self.inner.tasks.close();
        }
        // Startups register readers before their own tracked lifetime ends, so
        // shutdown cannot miss a reader published by an in-flight handshake.
        self.inner.tasks.wait().await;
        self.inner.factory.shutdown().await;
    }

    #[cfg(test)]
    pub(crate) fn with_connection_factory(mut self, factory: Arc<dyn ConnectionFactory>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("connection factories are installed before sharing a manager")
            .factory = factory;
        self
    }

    /// Join or start the pooled connection along `route`, once `admitted`
    /// allows it. It is asked under the pool's lock, which `invalidate` takes
    /// too, so a startup it admits is one any later invalidation sees.
    pub(crate) fn connection<'a>(
        &'a self,
        route: ResolvedRoute,
        workspace: &'a Path,
        cancellation: &'a CancellationToken,
        admitted: &'a (dyn Fn() -> Result<(), RemoteError> + Sync),
    ) -> BoxFuture<'a, Result<PreparedConnection, RemoteError>> {
        Box::pin(async move {
            let key = ConnectionKey {
                route: route.identity().clone(),
                workspace: workspace.to_path_buf(),
            };
            let slot = {
                let mut pool = self.pool();
                if self.inner.shutdown.is_cancelled() {
                    return Err(RemoteError::Cancelled);
                }
                admitted()?;
                if let Some(existing) = pool.get(&key) {
                    existing.clone()
                } else {
                    let manager = self.clone();
                    let workspace = workspace.to_path_buf();
                    let startup = self.inner.shutdown.child_token();
                    let stopped = startup.clone();
                    let started = self.inner.tasks.spawn(async move {
                        let connect = async { manager.connect(&route, &workspace, &stopped).await };
                        tokio::select! {
                            biased;
                            () = stopped.cancelled() => Err(if manager.inner.shutdown.is_cancelled() {
                                RemoteError::Cancelled
                            } else {
                                AuthorizationError::TargetRedefined.into()
                            }),
                            result = connect => result,
                        }
                    });
                    let connection = async move {
                        started.await.map_err(|error| {
                            RemoteError::task_failed("remote connection startup", error)
                        })?
                    }
                    .boxed()
                    .shared();
                    let slot = Arc::new(PooledSlot {
                        connection,
                        startup,
                    });
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
                    self.remove_slot(&key, &slot);
                    Err(error)
                }
            }
        })
    }

    pub(crate) fn is_current(&self, prepared: &PreparedConnection) -> bool {
        (self.pool().get(&prepared.key)).is_some_and(|slot| Arc::ptr_eq(slot, &prepared.slot))
    }

    pub(crate) fn discard(&self, prepared: &PreparedConnection) {
        self.remove_slot(&prepared.key, &prepared.slot);
    }

    fn remove_slot(&self, key: &ConnectionKey, slot: &Arc<PooledSlot>) {
        let mut pool = self.pool();
        if pool
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, slot))
        {
            pool.remove(key);
        }
    }

    /// Stop pooling the connections whose route names a revision `redefined`
    /// replaces, stopping those still starting. Connections already handed out
    /// keep running.
    pub(crate) fn invalidate(&self, redefined: &[TargetDefinition]) {
        self.pool().retain(|key, slot| {
            let replaced = crate::target::replaces(redefined, &key.route.revisions());
            if replaced {
                slot.startup.cancel();
            }
            !replaced
        });
    }

    async fn connect(
        &self,
        resolved_route: &ResolvedRoute,
        workspace: &Path,
        startup: &CancellationToken,
    ) -> Result<Session, RemoteError> {
        let Route { hops, destination } = resolved_route.route();
        let location = ExecutionLocation::named(destination.name.clone(), workspace.to_owned());
        // The destination's origin starts this connection's SSH process on its shim;
        // the hops after it are native jumps of that process.
        let (origin, hops) = match &destination.origin {
            None => (None, hops.as_slice()),
            Some(origin) => {
                let index = hops.iter().position(|hop| &hop.name == origin).ok_or(
                    RemoteError::Protocol(ProtocolError::Violation("origin missing from route")),
                )?;
                let prefix = ResolvedRoute::new(Route {
                    hops: hops[..index].to_vec(),
                    destination: hops[index].clone(),
                });
                // Once this startup stops, it starts no origin connection.
                let redefined = || AuthorizationError::TargetRedefined.into();
                let running = || {
                    (!startup.is_cancelled())
                        .then_some(())
                        .ok_or_else(redefined)
                };
                let cancellation = CancellationToken::new();
                let prepared = self
                    .connection(prefix, &hops[index].workspace, &cancellation, &running)
                    .await?;
                (Some(prepared.connection), &hops[index + 1..])
            }
        };
        let route = Route {
            hops: hops.to_vec(),
            destination: destination.clone(),
        };
        let mut transport = self
            .inner
            .factory
            .connect(ConnectionRequest {
                route,
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
                location,
                resolved_route.identity().revisions(),
                self.inner.prompts.clone(),
                &self.inner.tasks,
                self.inner.shutdown.child_token(),
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
        backend::Transport,
        client::test_transport,
        protocol::{Request, Response, read_frame, write_frame},
    };
    use crate::{target::TargetDefinition, tests::bounded};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::Semaphore;

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
                target: request.route.destination.name.to_string(),
                route: request
                    .route
                    .iter()
                    .map(|hop| hop.name.to_string())
                    .collect(),
                has_origin: request.origin.is_some(),
                workspace: request.workspace,
            });
            Box::pin(async move {
                if !self.invalid.load(Ordering::SeqCst) {
                    return Ok(test_transport(None));
                }
                Ok(Transport {
                    input: Box::new(tokio::io::sink()),
                    output: Box::new(tokio::io::empty()),
                    owner: Box::new(DropCounter(self.rejected_owners.clone(), None)),
                })
            })
        }
    }

    fn manager(factory: Arc<dyn ConnectionFactory>) -> RemoteManager {
        let prompts = Arc::new(RejectSensitivePrompts);
        RemoteManager::new(
            EmbeddedShimCatalog::default(),
            prompts,
            None,
            Default::default(),
        )
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
            bounded(self.hello.acquire()).await.unwrap().forget();
        }

        /// (transports started, transport owners dropped)
        pub fn counts(&self) -> (usize, usize) {
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

    fn route(mut hops: Vec<TargetDefinition>) -> ResolvedRoute {
        let destination = hops.pop().unwrap();
        ResolvedRoute::new(Route { hops, destination })
    }

    fn admitted() -> Result<(), RemoteError> {
        Ok(())
    }

    fn build_route() -> ResolvedRoute {
        route(vec![TargetDefinition::test("build", "/build", None)])
    }

    #[tokio::test]
    async fn evicted_connections_drain_accepted_output_before_shutdown() {
        use crate::{
            remote::{
                client::tests::{fixture_context, write_result},
                protocol::{PayloadEvent, RequestId},
            },
            session::SessionEvent,
            tool::output::{CaptureEvent, CaptureId},
        };

        struct ControlledFactory(std::sync::Mutex<Option<Transport>>);
        impl ConnectionFactory for ControlledFactory {
            fn connect(
                &self,
                _: ConnectionRequest,
            ) -> BoxFuture<'_, Result<Transport, RemoteError>> {
                let transport = self.0.lock().unwrap().take().unwrap();
                Box::pin(async { Ok(transport) })
            }
        }

        bounded(async {
            for successful in [true, false] {
                let runtime = crate::tests::TestRuntime::new().await;
                runtime
                    .jobs
                    .test_create(crate::job::JobSpec::test(runtime.agent.clone(), "payload"))
                    .await;
                let context = fixture_context(&runtime);
                let saved = runtime.jobs.output(context.job());
                let (client, mut shim) = tokio::io::duplex(4096);
                let (output, input) = tokio::io::split(client);
                let (owner, dropped) = tokio::sync::oneshot::channel::<()>();
                let manager = manager(Arc::new(ControlledFactory(std::sync::Mutex::new(Some(
                    Transport {
                        input: Box::new(input),
                        output: Box::new(output),
                        owner: Box::new(owner),
                    },
                )))));
                let cancellation = CancellationToken::new();
                let connecting = manager.connection(
                    build_route(),
                    Path::new("/build"),
                    &cancellation,
                    &admitted,
                );
                let handshake = async {
                    assert!(matches!(
                        read_frame::<_, Request>(&mut shim).await.unwrap(),
                        Some(Request::Hello)
                    ));
                    write_frame(&mut shim, &Response::Ready).await.unwrap();
                };
                let (prepared, ()) = tokio::join!(connecting, handshake);
                let prepared = prepared.unwrap();
                let abandoned = tokio::spawn({
                    let prepared = prepared.clone();
                    let context = context.clone();
                    async move {
                        prepared
                            .execute("read".into(), serde_json::json!({}), &context)
                            .await
                    }
                });
                assert!(matches!(
                    read_frame::<_, Request>(&mut shim).await.unwrap(),
                    Some(Request::Tool { .. })
                ));
                context.cancellation_token().cancel();
                assert!(matches!(
                    read_frame::<_, Request>(&mut shim).await.unwrap(),
                    Some(Request::Cancel { .. })
                ));
                assert!(matches!(
                    abandoned.await.unwrap(),
                    Err(RemoteError::Cancelled)
                ));

                let caller = fixture_context(&runtime);
                let completing =
                    prepared
                        .clone()
                        .execute("read".into(), serde_json::json!({}), &caller);
                tokio::pin!(completing);
                if successful {
                    // Register B, but do not poll its returned result until failure is
                    // published. Its success must still evict this broken transport.
                    tokio::select! {
                        result = &mut completing => panic!("unexpected completion: {result:?}"),
                        request = read_frame::<_, Request>(&mut shim) => {
                            assert!(matches!(request.unwrap(), Some(Request::Tool { .. })));
                        }
                    }
                }

                // Hold the shared database connection: A's capture open blocks, and an
                // append queued behind it; both accepted data chunks remain queued.
                let (reached, resume) = runtime.store.hold_connection();
                let accepted = tokio::spawn({
                    let (store, agent) = (runtime.store.clone(), runtime.agent.clone());
                    async move { store.append(agent, SessionEvent::AgentInterrupted).await }
                });
                reached.await.unwrap();
                for event in [
                    CaptureEvent::Open {
                        id: CaptureId::FIRST,
                        field: "/result/content".parse().unwrap(),
                        kind: crate::job::output::CaptureKind::Text,
                    },
                    CaptureEvent::Write {
                        id: CaptureId::FIRST,
                        data: b"accepted ".to_vec(),
                    },
                    CaptureEvent::Write {
                        id: CaptureId::FIRST,
                        data: b"after cancellation".to_vec(),
                    },
                ] {
                    write_frame(
                        &mut shim,
                        &Response::Payload {
                            request_id: RequestId::new(1),
                            event: PayloadEvent::Capture(event),
                        },
                    )
                    .await
                    .unwrap();
                }
                if successful {
                    write_result(
                        &mut shim,
                        RequestId::new(2),
                        Ok(crate::remote::protocol::RemoteToolOutput {
                            diagnostic: None,
                            value: serde_json::json!("second"),
                            images: Vec::new(),
                            captures: Vec::new(),
                            streams: Default::default(),
                        }),
                    )
                    .await;
                }
                drop(shim);
                assert!(
                    dropped.await.is_err(),
                    "transport owner must stop before persistence drains"
                );
                let result = (&mut completing).await;
                if successful {
                    assert_eq!(result.unwrap().value, "second");
                } else {
                    assert_eq!(
                        result
                            .unwrap_err()
                            .into_tool_error()
                            .diagnostic()
                            .context
                            .effects,
                        crate::tool::diagnostic::Effects::NotStarted
                    );
                }
                assert!(!manager.is_current(&prepared));
                let connection = Arc::downgrade(&prepared.connection);
                drop(prepared);
                assert!(
                    connection.upgrade().is_none(),
                    "no caller or pool owner may be needed to drain"
                );

                let closing = manager.shutdown();
                tokio::pin!(closing);
                assert!(
                    futures_util::poll!(&mut closing).is_pending(),
                    "session shutdown must await the accepted payload drain"
                );
                resume.send(()).unwrap();
                drop(accepted);
                closing.await;
                assert_eq!(
                    saved.test_bytes("/result/content").unwrap(),
                    b"accepted after cancellation"
                );
            }
        })
        .await;
    }

    #[tokio::test]
    async fn every_factory_uses_common_handshake_and_failed_slots_are_retryable() {
        let factory = Arc::new(RecordingFactory::default());
        factory.invalid.store(true, Ordering::SeqCst);
        let manager = manager(factory.clone());
        let cancellation = CancellationToken::new();
        let connect =
            || manager.connection(build_route(), Path::new("/build"), &cancellation, &admitted);
        let error = connect()
            .await
            .err()
            .unwrap()
            .into_tool_error()
            .diagnostic();
        assert_eq!(
            error.context.operation,
            crate::tool::diagnostic::Operation::Receive
        );
        assert_eq!(
            error.context.site,
            crate::tool::diagnostic::FailureSite::Host
        );
        assert_eq!(
            error.context.effects,
            crate::tool::diagnostic::Effects::NotStarted
        );
        assert!(
            matches!(error.cause, crate::tool::diagnostic::Cause::Message(message) if message.contains("invalid shim handshake"))
        );
        assert_eq!(factory.rejected_owners.load(Ordering::SeqCst), 1);
        assert!(manager.pool().is_empty());
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
        let first = manager.connection(build_route(), Path::new("/build"), &cancelled, &admitted);
        let joined = manager.connection(build_route(), Path::new("/build"), &survivor, &admitted);
        tokio::pin!(first, joined);
        assert!(futures_util::poll!(&mut first).is_pending());
        factory.wait_for_hello().await;
        assert!(futures_util::poll!(&mut joined).is_pending());
        cancelled.cancel();
        assert!(matches!(first.await, Err(RemoteError::Cancelled)));
        assert!(futures_util::poll!(&mut joined).is_pending());
        assert_eq!(factory.counts(), (1, 0));

        // A waiter arriving after the cancellation joins the same startup too.
        let second = manager.connection(build_route(), Path::new("/build"), &survivor, &admitted);
        tokio::pin!(second);
        assert!(futures_util::poll!(&mut second).is_pending());
        factory.ready.add_permits(1);
        bounded(joined).await.unwrap();
        let prepared = bounded(second).await.unwrap();
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
        let first =
            manager.connection(build_route(), Path::new("/build"), &cancellation, &admitted);
        let second =
            manager.connection(build_route(), Path::new("/build"), &cancellation, &admitted);
        tokio::pin!(first, second);
        assert!(futures_util::poll!(&mut first).is_pending());
        factory.wait_for_hello().await;
        assert!(futures_util::poll!(&mut second).is_pending());
        assert_eq!(factory.counts(), (1, 0));

        manager.shutdown().await;
        let (first, second) = bounded(async { tokio::join!(first, second) }).await;
        assert!(matches!(first, Err(RemoteError::Cancelled)));
        assert!(matches!(second, Err(RemoteError::Cancelled)));
        assert_eq!(factory.counts(), (1, 1));
        assert!(manager.pool().is_empty());
    }

    #[tokio::test]
    async fn factory_receives_only_route_after_ssh_origin() {
        let factory = Arc::new(RecordingFactory::default());
        let manager = manager(factory.clone());
        let origin = TargetDefinition::test("origin", "/origin", None);
        let mut via = TargetDefinition::test("via", "/via", None);
        via.origin = Some("origin".parse().unwrap());
        // The jump belongs to the SSH process the destination's origin starts.
        let mut destination = TargetDefinition::test("build", "/build", Some("via"));
        destination.origin = Some("origin".parse().unwrap());
        // Origins nest: deep's SSH process runs on build's shim.
        let mut deep = TargetDefinition::test("deep", "/deep", None);
        deep.origin = Some("build".parse().unwrap());
        let route = route(vec![origin, via, destination, deep]);
        let cancellation = CancellationToken::new();
        let prepared = manager.connection(route, Path::new("/override"), &cancellation, &admitted);
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
        let mut redefined = TargetDefinition::test("origin", "/origin", None);
        redefined.revision = 2;
        manager.invalidate(&[redefined]);
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

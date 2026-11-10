use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
};

use futures_util::future::{BoxFuture, FutureExt as _, Shared};
use tokio_util::task::TaskTracker;

use super::{
    artifact::EmbeddedShimCatalog,
    backend::{ConnectionFactory, ConnectionRequest},
    client::{Flown, Landed, PooledConnection},
    error::{RemoteError, RemoteFault},
};
use crate::job::CancellationToken;
use crate::target::{ConfiguredPasswords, Revisions, Route, TargetDefinition};
use crate::tool::authorization::AuthorizationError;
use skyhook_worker::protocol::Wire;
use skyhook_worker::{op::TargetPath, origin::ProcessEnvironment};

#[derive(Clone)]
pub(crate) struct RemoteManager {
    inner: Arc<RemoteInner>,
}

struct RemoteInner {
    factory: Arc<dyn ConnectionFactory>,
    shutdown: CancellationToken,
    tasks: TaskTracker,
    pool: Mutex<HashMap<Revisions, Arc<PooledSlot>>>,
}

struct PooledSlot {
    connection: Shared<BoxFuture<'static, Result<Arc<PooledConnection>, RemoteError>>>,
    /// Stops the startup once a redefinition replaces a target on its route, or the
    /// last caller awaiting it leaves.
    startup: CancellationToken,
    /// The callers awaiting the startup, counted under the pool's lock.
    waiting: AtomicUsize,
    /// The connections this one's shim carries as their origin, from when they start
    /// until they have ended.
    carried: TaskTracker,
}

/// A caller awaiting a slot's startup. The last to leave one still starting stops it
/// and unpools its slot: nothing would end a startup that hangs, and the next call
/// would join it.
struct Waiting<'a> {
    manager: &'a RemoteManager,
    key: &'a Revisions,
    slot: &'a Arc<PooledSlot>,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        let mut pool = self.manager.pool();
        let last = self.slot.waiting.fetch_sub(1, Ordering::Relaxed) == 1;
        if last && self.slot.connection.peek().is_none() {
            self.slot.startup.cancel();
            unpool(&mut pool, self.key, self.slot);
        }
    }
}

/// Remove `slot` from `pool`, unless another has taken its place under `key`.
fn unpool(pool: &mut HashMap<Revisions, Arc<PooledSlot>>, key: &Revisions, slot: &Arc<PooledSlot>) {
    if (pool.get(key)).is_some_and(|current| Arc::ptr_eq(current, slot)) {
        pool.remove(key);
    }
}

/// A route's pooled connection, its slot in the pool and the connection.
type Pooled = (Revisions, Arc<PooledSlot>, Arc<PooledConnection>);

/// A pooled connection along a route, for calls in one workspace there.
#[derive(Clone)]
pub(crate) struct PreparedConnection {
    manager: RemoteManager,
    route: Revisions,
    slot: Arc<PooledSlot>,
    connection: Arc<PooledConnection>,
    workspace: TargetPath,
}

impl PreparedConnection {
    /// What the worker reported about its machine when this connection opened.
    pub(crate) fn facts(&self) -> &skyhook_worker::target::Facts {
        self.connection.facts()
    }

    /// The workspace its calls run in, as the worker resolved it.
    pub(crate) fn workspace(&self) -> &TargetPath {
        &self.workspace
    }

    /// The revision of each target on the route the connection was admitted at.
    pub(crate) fn admitted(&self) -> &Revisions {
        self.connection.admitted()
    }

    /// Start `operation` on the connected machine. A connection that fails
    /// answering it leaves the pool, so the next call connects afresh.
    pub(crate) fn run<O: Wire>(
        &self,
        operation: O,
    ) -> Result<skyhook_worker::op::Running<O, RemoteError>, RemoteError> {
        let running = self.connection.run(operation, self.release());
        self.discard_failed(running)
    }

    /// Start `operation` on the connected machine in the call that resolves `paths`
    /// there, as [`PooledConnection::contain`] does.
    pub(crate) async fn contain<O: Wire>(
        &self,
        paths: skyhook_worker::op::Resolve,
        operation: O,
    ) -> Result<(Landed, Flown<O>), RemoteError> {
        let contained = self.connection.contain(paths, operation, self.release());
        self.discard_failed(contained.await)
    }

    /// What drops this connection from the pool once a call on it finds it failed.
    fn release(&self) -> impl FnOnce() + Send + 'static {
        let released = self.clone();
        move || released.manager.discard(&released)
    }

    fn discard_failed<T>(&self, started: Result<T, RemoteError>) -> Result<T, RemoteError> {
        if started.is_err() && self.connection.is_failed() {
            self.manager.discard(self);
        }
        started
    }
}

impl RemoteManager {
    /// A manager whose startups and connections stop once `shutdown` is cancelled,
    /// as the session's own token is when its journal fails. Its connections read
    /// a configured target's password from `passwords`.
    pub(crate) fn new(
        catalog: EmbeddedShimCatalog,
        passwords: ConfiguredPasswords,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            inner: Arc::new(RemoteInner {
                factory: Arc::new(super::backend::Backend::new(catalog, passwords)),
                shutdown,
                tasks: TaskTracker::new(),
                pool: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub(crate) async fn agent_environment(&self) -> Result<ProcessEnvironment, RemoteError> {
        if self.inner.shutdown.is_cancelled() {
            return Err(RemoteFault::Cancelled.into());
        }
        self.inner.factory.agent_environment().await
    }
    fn pool(&self) -> std::sync::MutexGuard<'_, HashMap<Revisions, Arc<PooledSlot>>> {
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

    /// The pooled connection along `route`, joined or started once `admitted`
    /// allows it, for calls in `workspace` there, as its worker resolves it.
    pub(crate) async fn connection(
        &self,
        route: Route,
        workspace: Vec<TargetPath>,
        cancellation: &CancellationToken,
        admitted: &(dyn Fn() -> Result<(), RemoteError> + Sync),
    ) -> Result<PreparedConnection, RemoteError> {
        let (key, slot, connection) = self.pooled(route, cancellation, admitted).await?;
        let resolved = tokio::select! {
            resolved = connection.workspace(&workspace) => resolved,
            () = cancellation.cancelled() => return Err(RemoteFault::Cancelled.into()),
        };
        if resolved.is_err() && connection.is_failed() {
            self.remove_slot(&key, &slot);
        }
        Ok(PreparedConnection {
            manager: self.clone(),
            route: key,
            slot,
            connection,
            workspace: resolved?,
        })
    }

    /// Join or start the pooled connection along `route`, once `admitted`
    /// allows it. It is asked under the pool's lock, which `invalidate` takes
    /// too, so a startup it admits is one any later invalidation sees.
    fn pooled<'a>(
        &'a self,
        route: Route,
        cancellation: &'a CancellationToken,
        admitted: &'a (dyn Fn() -> Result<(), RemoteError> + Sync),
    ) -> BoxFuture<'a, Result<Pooled, RemoteError>> {
        Box::pin(async move {
            let key = route.identity();
            let slot = {
                let mut pool = self.pool();
                if self.inner.shutdown.is_cancelled() {
                    return Err(RemoteFault::Cancelled.into());
                }
                admitted()?;
                let slot = if let Some(existing) = pool.get(&key) {
                    existing.clone()
                } else {
                    let manager = self.clone();
                    let startup = self.inner.shutdown.child_token();
                    let stopped = startup.clone();
                    let carried = TaskTracker::new();
                    let carrying = carried.clone();
                    let started = self.inner.tasks.spawn(async move {
                        let connect = manager.connect(&route, &stopped, carrying);
                        tokio::select! {
                            biased;
                            () = stopped.cancelled() => Err(if manager.inner.shutdown.is_cancelled() {
                                RemoteFault::Cancelled.into()
                            } else {
                                AuthorizationError::TargetRedefined.into()
                            }),
                            result = connect => result,
                        }
                    });
                    let connection = async move {
                        started
                            .await
                            .map_err(|error| RemoteFault::StartupFailed(Arc::new(error)))?
                    }
                    .boxed()
                    .shared();
                    let slot = Arc::new(PooledSlot {
                        connection,
                        startup,
                        waiting: AtomicUsize::new(0),
                        carried,
                    });
                    pool.insert(key.clone(), slot.clone());
                    slot
                };
                slot.waiting.fetch_add(1, Ordering::Relaxed);
                slot
            };
            let waiting = Waiting {
                manager: self,
                key: &key,
                slot: &slot,
            };
            let result = tokio::select! {
                result = slot.connection.clone() => result,
                () = cancellation.cancelled() => return Err(RemoteFault::Cancelled.into()),
            };
            drop(waiting);
            match result {
                Ok(connection) => Ok((key, slot, connection)),
                Err(error) => {
                    self.remove_slot(&key, &slot);
                    Err(error)
                }
            }
        })
    }

    pub(crate) fn is_current(&self, prepared: &PreparedConnection) -> bool {
        (self.pool().get(&prepared.route)).is_some_and(|slot| Arc::ptr_eq(slot, &prepared.slot))
    }

    pub(crate) fn discard(&self, prepared: &PreparedConnection) {
        self.remove_slot(&prepared.route, &prepared.slot);
    }

    fn remove_slot(&self, key: &Revisions, slot: &Arc<PooledSlot>) {
        unpool(&mut self.pool(), key, slot);
    }

    /// Stop pooling the connections whose route names a revision `redefined`
    /// replaces, stopping those still starting. Connections already handed out
    /// keep running.
    pub(crate) fn invalidate(&self, redefined: &[TargetDefinition]) {
        self.pool().retain(|route, slot| {
            let replaced = crate::target::replaces(redefined, route);
            if replaced {
                slot.startup.cancel();
            }
            !replaced
        });
    }

    /// Connect along `route`. The connection ends once its last owner drops it, or
    /// once the session shuts down and the connections it has `carried` as their
    /// origin have ended: ending it sooner would stop their shims before they finish.
    async fn connect(
        &self,
        route: &Route,
        startup: &CancellationToken,
        carried: TaskTracker,
    ) -> Result<Arc<PooledConnection>, RemoteError> {
        let destination = &route.destination;
        // The destination's origin starts this connection on its shim.
        let origin = match &route.origin {
            None => None,
            Some(origin) => {
                // Once this startup stops, it starts no origin connection.
                let redefined = || AuthorizationError::TargetRedefined.into();
                let running = || {
                    (!startup.is_cancelled())
                        .then_some(())
                        .ok_or_else(redefined)
                };
                let cancellation = CancellationToken::new();
                let pooled = self.pooled((**origin).clone(), &cancellation, &running);
                let (_, slot, connection) = pooled.await?;
                Some((connection, slot.carried.token()))
            }
        };
        let mut transport = self
            .inner
            .factory
            .connect(ConnectionRequest {
                route: route.clone(),
                origin: origin.as_ref().map(|(origin, _)| origin.clone()),
            })
            .await?;
        // Retain the origin's session for the whole child stream, independently
        // of the backend's own transport lifetime guards, and count among those it
        // carries until this connection has ended.
        transport.owner = Box::new((transport.owner, origin));
        let ending = CancellationToken::new();
        let connection = PooledConnection::from_transport(
            transport,
            destination.connection.kind(),
            destination.name.clone(),
            route.identity(),
            &self.inner.tasks,
            ending.clone(),
        )
        .await?;
        let shutdown = self.inner.shutdown.clone();
        self.inner.tasks.spawn(async move {
            let emptied = async {
                shutdown.cancelled().await;
                carried.close();
                carried.wait().await;
            };
            tokio::select! {
                () = ending.cancelled() => {}
                () = emptied => ending.cancel(),
            }
        });
        Ok(Arc::new(connection))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::target::TargetDefinition;
    use crate::tests::bounded;
    use skyhook_worker::{
        origin::transport::Transport,
        protocol::{CallId, ClientFrame, FrameReader, Op, PREAMBLE, WorkerFrame, write_frame},
    };
    use std::assert_matches;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::{io::AsyncReadExt as _, sync::Semaphore};

    impl RemoteManager {
        pub(crate) fn with_connection_factory(
            mut self,
            factory: Arc<dyn ConnectionFactory>,
        ) -> Self {
            Arc::get_mut(&mut self.inner)
                .expect("connection factories are installed before sharing a manager")
                .factory = factory;
            self
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct RecordedRequest {
        target: String,
        route: Vec<String>,
        has_origin: bool,
    }

    /// What a worker answers call `id` when it resolves a workspace: its segments
    /// joined.
    fn workspace_answer(id: CallId, op: &Op) -> Option<WorkerFrame> {
        let Op::Workspace(workspace) = op else {
            return None;
        };
        let joined: std::path::PathBuf =
            workspace.segments.iter().map(TargetPath::as_str).collect();
        let joined = TargetPath::new(joined.to_string_lossy());
        let outcome = skyhook_worker::op::Workspace::outcome(Ok(joined));
        Some(WorkerFrame::Done { id, outcome })
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

    /// Starts fake shims, recording each request; a shim holds its handshake until
    /// `ready` has a permit, then answers no call but those resolving a workspace,
    /// only the connection's end, which it records. While `invalid`, transports never
    /// answer the handshake.
    pub(crate) struct ShimFactory {
        pub starts: AtomicUsize,
        hello: Arc<Semaphore>,
        calls: Arc<Semaphore>,
        pub ready: Arc<Semaphore>,
        dropped: Arc<AtomicUsize>,
        invalid: AtomicBool,
        requests: std::sync::Mutex<Vec<RecordedRequest>>,
        origins: std::sync::Mutex<Vec<std::sync::Weak<PooledConnection>>>,
        /// The targets whose connections were ended, in that order.
        ended: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl ShimFactory {
        /// Shims whose handshake waits for `ready`.
        pub fn new() -> Arc<Self> {
            Self::with_ready(0)
        }

        /// Shims that answer their handshake at once.
        pub fn answering() -> Arc<Self> {
            Self::with_ready(Semaphore::MAX_PERMITS)
        }

        fn with_ready(permits: usize) -> Arc<Self> {
            Arc::new(Self {
                starts: AtomicUsize::new(0),
                hello: Arc::new(Semaphore::new(0)),
                calls: Arc::new(Semaphore::new(0)),
                ready: Arc::new(Semaphore::new(permits)),
                dropped: Arc::new(AtomicUsize::new(0)),
                invalid: AtomicBool::new(false),
                requests: Default::default(),
                origins: Default::default(),
                ended: Default::default(),
            })
        }

        pub async fn wait_for_hello(&self) {
            bounded(self.hello.acquire()).await.unwrap().forget();
        }

        pub async fn wait_for_call(&self) {
            bounded(self.calls.acquire()).await.unwrap().forget();
        }

        /// (transports started, transport owners dropped)
        pub fn counts(&self) -> (usize, usize) {
            let starts = self.starts.load(Ordering::SeqCst);
            (starts, self.dropped.load(Ordering::SeqCst))
        }
    }

    impl ConnectionFactory for ShimFactory {
        fn connect(
            &self,
            request: ConnectionRequest,
        ) -> BoxFuture<'_, Result<Transport, RemoteError>> {
            if let Some(origin) = &request.origin {
                self.origins.lock().unwrap().push(Arc::downgrade(origin));
            }
            self.requests.lock().unwrap().push(RecordedRequest {
                target: request.route.destination.name.to_string(),
                route: (request.route.jumps.iter().map(|jump| &jump.name))
                    .chain([&request.route.destination.name])
                    .map(ToString::to_string)
                    .collect(),
                has_origin: request.origin.is_some(),
            });
            Box::pin(async move {
                self.starts.fetch_add(1, Ordering::SeqCst);
                if self.invalid.load(Ordering::SeqCst) {
                    return Ok(Transport {
                        input: Box::new(tokio::io::sink()),
                        output: Box::new(tokio::io::empty()),
                        owner: Box::new(DropCounter(self.dropped.clone(), None)),
                    });
                }
                let (hello, ready, calls) =
                    (self.hello.clone(), self.ready.clone(), self.calls.clone());
                let (target, ended) = (request.route.destination.name, self.ended.clone());
                let (client, shim) = tokio::io::duplex(4096);
                let shim = tokio::spawn(async move {
                    let (frames, mut shim) = tokio::io::split(shim);
                    let mut frames = FrameReader::new(frames);
                    let request = frames.next::<ClientFrame>().await;
                    if !matches!(request, Ok(Some(ClientFrame::Hello))) {
                        return;
                    }
                    hello.add_permits(1);
                    ready.acquire().await.unwrap().forget();
                    if write_frame(&mut shim, &WorkerFrame::Ready(facts()))
                        .await
                        .is_ok()
                    {
                        while let Ok(Some(frame)) = frames.next::<ClientFrame>().await {
                            match frame {
                                ClientFrame::Call { id, op, .. } => match workspace_answer(id, &op)
                                {
                                    Some(answer) => {
                                        let _ = write_frame(&mut shim, &answer).await;
                                    }
                                    None => calls.add_permits(1),
                                },
                                ClientFrame::End => {
                                    ended.lock().unwrap().push(target.to_string());
                                    let _ = write_frame(&mut shim, &WorkerFrame::Ended).await;
                                    return;
                                }
                                _ => {}
                            }
                        }
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

    /// Serves each connection with this process's own worker, as a shim would,
    /// recording the operation each call names.
    #[derive(Default)]
    pub(crate) struct ServingFactory(Arc<std::sync::Mutex<Vec<String>>>, Arc<HeldReads>);

    /// Reads a worker is never given, once they are held, and what became of them.
    #[derive(Default)]
    struct HeldReads {
        holding: AtomicBool,
        held: tokio::sync::Notify,
        cancelled: tokio::sync::Notify,
    }

    impl ServingFactory {
        /// The operations called since last asked, each as its wire variant names it.
        pub(crate) fn served(&self) -> Vec<String> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }

        /// Leave the reads called from now on unanswered, and note their cancelling.
        pub(crate) fn hold_reads(&self) {
            self.1.holding.store(true, Ordering::SeqCst);
        }

        /// Once a read was left unanswered.
        pub(crate) async fn read_held(&self) {
            bounded(self.1.held.notified()).await;
        }

        /// Once a call was cancelled while reads are held.
        pub(crate) async fn call_cancelled(&self) {
            bounded(self.1.cancelled.notified()).await;
        }
    }

    impl ConnectionFactory for ServingFactory {
        fn connect(
            &self,
            request: ConnectionRequest,
        ) -> BoxFuture<'_, Result<Transport, RemoteError>> {
            let settings = skyhook_worker::target::Settings {
                authorization_root: request.route.destination.workspace.clone().into(),
            };
            let (client, tapped) = tokio::io::duplex(64 * 1024);
            let (sent, mut answers) = tokio::io::split(tapped);
            let (near, shim) = tokio::io::duplex(64 * 1024);
            let (mut answered, mut forwarded) = tokio::io::split(near);
            let (shim_input, shim_output) = tokio::io::split(shim);
            let (served, reads) = (self.0.clone(), self.1.clone());
            let serving = tokio::spawn(async move {
                let serve =
                    skyhook_worker::protocol::serve::serve(shim_input, shim_output, settings);
                let tap = async move {
                    let mut sent = FrameReader::new(sent);
                    while let Ok(Some(frame)) = sent.next::<ClientFrame>().await {
                        let called = match &frame {
                            ClientFrame::Call {
                                op,
                                contained: None,
                                ..
                            } => Some(("", op)),
                            ClientFrame::Call { op, .. } => Some(("Contained", op)),
                            _ => None,
                        };
                        // A call's workspace resolves once for the connection.
                        if let Some((prefix, op)) = called
                            && !matches!(**op, Op::Workspace(_))
                            && let Ok(serde_json::Value::Object(op)) = serde_json::to_value(op)
                        {
                            let named = op.keys().map(|name| format!("{prefix}{name}"));
                            served.lock().unwrap().extend(named);
                        }
                        match (&frame, reads.holding.load(Ordering::SeqCst)) {
                            (ClientFrame::Call { op, .. }, true) if matches!(**op, Op::Read(_)) => {
                                reads.held.notify_one();
                                continue;
                            }
                            (ClientFrame::Cancel { .. }, true) => reads.cancelled.notify_one(),
                            _ => {}
                        }
                        if write_frame(&mut forwarded, &frame).await.is_err() {
                            return;
                        }
                    }
                };
                let answer = async {
                    answered.read_exact(&mut [0; PREAMBLE.len()]).await?;
                    tokio::io::copy(&mut answered, &mut answers).await
                };
                drop(tokio::join!(serve, tap, answer));
            });
            let (output, input) = tokio::io::split(client);
            Box::pin(async move {
                Ok(Transport {
                    input: Box::new(input),
                    output: Box::new(output),
                    owner: Box::new(tokio_util::task::AbortOnDropHandle::new(serving)),
                })
            })
        }
    }

    fn manager(factory: Arc<dyn ConnectionFactory>) -> RemoteManager {
        RemoteManager::new(Default::default(), Default::default(), Default::default())
            .with_connection_factory(factory)
    }

    /// The route to the last of `targets`.
    fn route(targets: Vec<TargetDefinition>) -> Route {
        let name = targets.last().unwrap().name.clone();
        let registry = crate::target::TargetRegistry::from_definitions(targets).unwrap();
        registry.route(&name).unwrap()
    }

    fn admitted() -> Result<(), RemoteError> {
        Ok(())
    }

    fn berlin() -> skyhook_worker::target::TimeZone {
        skyhook_worker::target::TimeZone {
            name: Some("Europe/Berlin".into()),
            offset: 3600,
        }
    }

    /// What a fake shim reports: a Linux machine in Berlin.
    pub(crate) fn facts() -> skyhook_worker::target::Facts {
        let path = skyhook_worker::op::TargetPath::new("/build");
        skyhook_worker::target::Facts {
            platform: skyhook_worker::target::Platform::current(),
            zone: berlin(),
            authorization_root: path,
        }
    }

    fn build_route() -> Route {
        route(vec![crate::target::tests::definition(
            "build", "/build", None,
        )])
    }

    /// A connection whose transport fails under a call leaves the pool, and the
    /// call ends with that failure; a call dropped before its end is cancelled.
    #[tokio::test]
    async fn failed_connections_leave_the_pool_and_dropped_calls_cancel() {
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

        let (client, shim) = tokio::io::duplex(4096);
        let (frames, mut shim) = tokio::io::split(shim);
        let mut frames = FrameReader::new(frames);
        let (output, input) = tokio::io::split(client);
        let manager = manager(Arc::new(ControlledFactory(std::sync::Mutex::new(Some(
            Transport {
                input: Box::new(input),
                output: Box::new(output),
                owner: Box::new(()),
            },
        )))));
        let cancellation = CancellationToken::new();
        let connecting = manager.connection(
            build_route(),
            vec!["/build".into()],
            &cancellation,
            &admitted,
        );
        let handshake = async {
            let hello = frames.next::<ClientFrame>().await.unwrap();
            assert_matches!(hello, Some(ClientFrame::Hello));
            write_frame(&mut shim, &WorkerFrame::Ready(facts()))
                .await
                .unwrap();
            let Some(ClientFrame::Call { id, op, .. }) = frames.next().await.unwrap() else {
                panic!("expected the workspace's resolution");
            };
            write_frame(&mut shim, &workspace_answer(id, &op).unwrap())
                .await
                .unwrap();
        };
        let (prepared, ()) = bounded(async { tokio::join!(connecting, handshake) }).await;
        let prepared = prepared.unwrap();
        let resolve = || skyhook_worker::op::Resolve {
            workspace: skyhook_worker::op::TargetPath::new("/build"),
            paths: Vec::new(),
        };
        let running = prepared.run(resolve()).unwrap();
        let call = frames.next::<ClientFrame>().await.unwrap();
        let Some(ClientFrame::Call { id, .. }) = call else {
            panic!("expected the call");
        };
        drop(running);
        let cancel = bounded(frames.next::<ClientFrame>()).await.unwrap();
        assert_matches!(cancel, Some(ClientFrame::Cancel { id: cancelled }) if cancelled == id);

        let running = prepared.run(resolve()).unwrap();
        drop((frames, shim));
        let ended = bounded(running.end()).await.unwrap_err();
        let diagnostic = crate::tool::ToolError::from(ended).diagnostic();
        assert_eq!(
            diagnostic.context.request,
            skyhook_tools::diagnostic::RequestEffects::MayHaveExecuted
        );
        assert!(!manager.is_current(&prepared));
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn every_factory_uses_common_handshake_and_failed_slots_are_retryable() {
        let factory = ShimFactory::answering();
        factory.invalid.store(true, Ordering::SeqCst);
        let manager = manager(factory.clone());
        let cancellation = CancellationToken::new();
        let connect = || {
            manager.connection(
                build_route(),
                vec!["/build".into()],
                &cancellation,
                &admitted,
            )
        };
        let failed = connect().await.err().unwrap();
        let handshake = skyhook_worker::protocol::Violation::InvalidHandshake;
        assert_matches!(
            &failed.fault,
            RemoteFault::Transport(skyhook_worker::origin::error::TransportError::Protocol(violation))
                if *violation == handshake
        );
        let error = crate::tool::ToolError::from(failed).diagnostic();
        assert_eq!(
            error.context.operation,
            skyhook_worker::diagnostic::Operation::Receive
        );
        assert_eq!(
            error.context.site,
            skyhook_tools::diagnostic::FailureSite::Host
        );
        assert_eq!(
            error.context.effects,
            skyhook_worker::diagnostic::Effects::NotStarted
        );
        assert_matches!(
            error.cause,
            skyhook_tools::diagnostic::Cause::Connection {
                fault: skyhook_tools::diagnostic::ConnectionFault::Protocol,
                ..
            }
        );
        assert_eq!(factory.counts(), (1, 1));
        assert!(manager.pool().is_empty());
        factory.invalid.store(false, Ordering::SeqCst);
        let first = connect().await.unwrap();
        let second = connect().await.unwrap();
        assert!(Arc::ptr_eq(&first.connection, &second.connection));
        // A connection keeps the facts its worker reported in its handshake.
        assert_eq!(first.facts(), &facts());
        assert_eq!(factory.requests.lock().unwrap().len(), 2);
        manager.shutdown().await;
    }

    /// A startup stops with the last caller awaiting it, so the next call starts
    /// afresh; while another waits it goes on, shared with those arriving later.
    #[tokio::test]
    async fn a_startup_stops_with_its_last_waiter_and_is_shared_until_then() {
        let factory = ShimFactory::new();
        let manager = manager(factory.clone());
        let (cancelled, survivor) = (CancellationToken::new(), CancellationToken::new());
        let alone = manager.connection(build_route(), vec!["/build".into()], &cancelled, &admitted);
        tokio::pin!(alone);
        assert!(futures_util::poll!(&mut alone).is_pending());
        factory.wait_for_hello().await;
        cancelled.cancel();
        let stopped = |ended: Result<PreparedConnection, RemoteError>| {
            matches!(
                ended.err().map(|ended| ended.fault),
                Some(RemoteFault::Cancelled)
            )
        };
        assert!(stopped(alone.await));
        assert!(manager.pool().is_empty());
        bounded(async {
            while factory.counts() != (1, 1) {
                tokio::task::yield_now().await;
            }
        })
        .await;

        let cancelled = CancellationToken::new();
        let first = manager.connection(build_route(), vec!["/build".into()], &cancelled, &admitted);
        let joined = manager.connection(build_route(), vec!["/build".into()], &survivor, &admitted);
        tokio::pin!(first, joined);
        assert!(futures_util::poll!(&mut first).is_pending());
        factory.wait_for_hello().await;
        assert!(futures_util::poll!(&mut joined).is_pending());
        cancelled.cancel();
        assert!(stopped(first.await));
        assert!(futures_util::poll!(&mut joined).is_pending());
        assert_eq!(factory.counts(), (2, 1));

        // A waiter arriving after the cancellation joins the same startup too.
        let second = manager.connection(build_route(), vec!["/build".into()], &survivor, &admitted);
        tokio::pin!(second);
        assert!(futures_util::poll!(&mut second).is_pending());
        factory.ready.add_permits(1);
        bounded(joined).await.unwrap();
        let prepared = bounded(second).await.unwrap();
        assert_eq!(factory.counts(), (2, 1));
        drop(prepared);
        manager.shutdown().await;
        assert_eq!(factory.counts(), (2, 2));
    }

    #[tokio::test]
    async fn shutdown_drops_transport_during_shared_pending_handshake() {
        let factory = ShimFactory::new();
        let manager = manager(factory.clone());
        let cancellation = CancellationToken::new();
        let first = manager.connection(
            build_route(),
            vec!["/build".into()],
            &cancellation,
            &admitted,
        );
        let second = manager.connection(
            build_route(),
            vec!["/build".into()],
            &cancellation,
            &admitted,
        );
        tokio::pin!(first, second);
        assert!(futures_util::poll!(&mut first).is_pending());
        factory.wait_for_hello().await;
        assert!(futures_util::poll!(&mut second).is_pending());
        assert_eq!(factory.counts(), (1, 0));

        manager.shutdown().await;
        let (first, second) = bounded(async { tokio::join!(first, second) }).await;
        for ended in [first, second] {
            let fault = ended.err().map(|ended| ended.fault);
            assert_matches!(fault, Some(RemoteFault::Cancelled));
        }
        assert_eq!(factory.counts(), (1, 1));
        assert!(manager.pool().is_empty());
    }

    /// Calls in two workspaces along one route share its one connection, each
    /// prepared for its own workspace as the worker resolved it, resolved once.
    #[tokio::test]
    async fn two_workspaces_share_one_connection() {
        let factory = ShimFactory::answering();
        let manager = manager(factory.clone());
        let cancellation = CancellationToken::new();
        let prepare = |workspace: &str| {
            let workspace = vec!["/build".into(), workspace.into()];
            manager.connection(build_route(), workspace, &cancellation, &admitted)
        };
        let (first, second) = tokio::join!(prepare("one"), prepare("/two"));
        let (first, second) = (first.unwrap(), second.unwrap());
        assert!(Arc::ptr_eq(&first.connection, &second.connection));
        assert_eq!(first.workspace(), &TargetPath::new("/build/one"));
        assert_eq!(second.workspace(), &TargetPath::new("/two"));
        let again = prepare("one").await.unwrap();
        assert_eq!(again.workspace(), first.workspace());
        assert_eq!(factory.counts(), (1, 0));
        drop((first, second, again));
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn factory_receives_only_route_after_its_origin() {
        let factory = ShimFactory::answering();
        let manager = manager(factory.clone());
        let origin = crate::target::tests::definition("origin", "/origin", None);
        let mut via = crate::target::tests::definition("via", "/via", None);
        via.origin = Some("origin".parse().unwrap());
        // The jump belongs to the SSH process the destination's origin starts.
        let mut destination = crate::target::tests::definition("build", "/build", Some("via"));
        destination.origin = Some("origin".parse().unwrap());
        // Origins nest: deep's WinRM client runs on build's shim, which SSH reached.
        let mut deep = crate::target::tests::winrm("deep", Some("build"));
        deep.os = skyhook_worker::target::Platform::current().os;
        let route = route(vec![origin, via, destination, deep]);
        let cancellation = CancellationToken::new();
        let prepared =
            manager.connection(route, vec!["/override".into()], &cancellation, &admitted);
        let prepared = prepared.await.unwrap();
        assert_eq!(prepared.workspace(), &TargetPath::new("/override"));
        let expected = [
            ("origin", vec!["origin"], false),
            ("build", vec!["via", "build"], true),
            ("deep", vec!["deep"], true),
        ]
        .map(|(target, route, has_origin)| RecordedRequest {
            target: target.into(),
            route: route.into_iter().map(String::from).collect(),
            has_origin,
        });
        assert_eq!(*factory.requests.lock().unwrap(), expected);
        let origin = factory.origins.lock().unwrap()[0].clone();
        let mut redefined = crate::target::tests::definition("origin", "/origin", None);
        redefined.revision = 2;
        manager.invalidate(&[redefined]);
        assert!(
            origin.upgrade().is_some(),
            "child stream must retain its origin"
        );
        // Still held, each connection ends before the origin whose shim carries it.
        manager.shutdown().await;
        assert_eq!(*factory.ended.lock().unwrap(), ["deep", "build", "origin"]);
        assert!(
            origin.upgrade().is_none(),
            "shutdown must release transport owners"
        );
    }
}

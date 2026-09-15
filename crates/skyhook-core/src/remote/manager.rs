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
    target::{ResolvedRoute, RouteIdentity, TargetDefinition},
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
        let destination = resolved_route.destination();
        let target = destination.name.as_str();
        let route = resolved_route.definitions();
        let (origin, hops) = if destination.origin == crate::target::ROOT_TARGET {
            (None, route)
        } else {
            let index = route
                .iter()
                .position(|hop| hop.name == destination.origin)
                .ok_or_else(|| RemoteError::Protocol("origin missing from route".into()))?;
            let prefix = ResolvedRoute::from_definitions(route[..=index].to_vec())
                .expect("inclusive route prefix is nonempty");
            let cancellation = CancellationToken::new();
            let prepared = self
                .connection(prefix, &route[index].workspace, &cancellation)
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
mod tests {
    use super::*;
    use crate::remote::{
        RejectSensitivePrompts, SecretValue, SensitivePrompt, SensitivePromptFuture,
        backend::{ConnectionFactory, ConnectionRequest, Transport},
        client::test_transport,
        protocol::{PROTOCOL_VERSION, Request, Response, read_frame, write_frame},
    };
    use crate::{
        target::{
            SshOptions, TargetAuth, TargetConfig, TargetConfigType, TargetDefinition, TargetSource,
        },
        tool::policy::AllowAll,
    };
    use std::process::Stdio;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
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
                target: request.target,
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

    struct PendingHandshakeFactory {
        starts: AtomicUsize,
        hello: Arc<Semaphore>,
        ready: Arc<Semaphore>,
        dropped: Arc<AtomicUsize>,
    }

    impl PendingHandshakeFactory {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                starts: AtomicUsize::new(0),
                hello: Arc::new(Semaphore::new(0)),
                ready: Arc::new(Semaphore::new(0)),
                dropped: Arc::new(AtomicUsize::new(0)),
            })
        }

        async fn wait_for_hello(&self) {
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
                    if !matches!(request, Ok(Some(Request::Hello { .. }))) {
                        return;
                    }
                    hello.add_permits(1);
                    ready.acquire().await.unwrap().forget();
                    let response = Response::Ready {
                        version: PROTOCOL_VERSION,
                    };
                    if write_frame(&mut shim, &response).await.is_ok() {
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
    async fn cancelling_one_waiter_preserves_shared_pending_handshake() {
        let factory = PendingHandshakeFactory::new();
        let manager = manager(factory.clone());
        let (cancelled, survivor) = (CancellationToken::new(), CancellationToken::new());
        let first = manager.connection(build_route(), Path::new("/build"), &cancelled);
        let second = manager.connection(build_route(), Path::new("/build"), &survivor);
        tokio::pin!(first, second);
        assert!(futures_util::poll!(&mut first).is_pending());
        factory.wait_for_hello().await;
        assert!(futures_util::poll!(&mut second).is_pending());

        cancelled.cancel();
        assert!(matches!(first.await, Err(RemoteError::Cancelled)));
        assert_eq!(factory.counts(), (1, 0));
        assert!(futures_util::poll!(&mut second).is_pending());

        factory.ready.add_permits(1);
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
    async fn factory_receives_only_route_after_credential_origin() {
        let factory = Arc::new(RecordingFactory::default());
        let manager = manager(factory.clone());
        let origin = TargetDefinition::test("origin", "/origin", None);
        let mut via = TargetDefinition::test("via", "/via", Some("origin"));
        via.origin = "origin".into();
        let mut destination = TargetDefinition::test("build", "/build", Some("via"));
        destination.origin = "origin".into();
        let route = route(vec![origin, via, destination]);
        let cancellation = CancellationToken::new();
        let prepared = manager.connection(route, Path::new("/override"), &cancellation);
        let prepared = prepared.await.unwrap();
        let expected = [
            ("origin", vec!["origin"], false, "/origin"),
            ("build", vec!["via", "build"], true, "/override"),
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

    struct Prompts(AtomicUsize);

    impl SensitivePromptHandler for Prompts {
        fn prompt(&self, prompt: SensitivePrompt) -> SensitivePromptFuture {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let kind = crate::remote::SensitivePromptKind::KeyPassphrase;
                assert_eq!(prompt.kind, kind, "unexpected prompt: {prompt:?}");
                Ok(SecretValue::new("fixture-passphrase".into()))
            })
        }
    }

    struct Server {
        child: tokio::process::Child,
        directory: tempfile::TempDir,
        port: u16,
        user: String,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.child.start_kill();
        }
    }

    impl Server {
        async fn start() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let path = |name: &str| directory.path().join(name);
            for (name, password) in [
                ("host", ""),
                ("first", ""),
                ("second", "fixture-passphrase"),
            ] {
                let mut keygen = tokio::process::Command::new("ssh-keygen");
                keygen
                    .args(["-q", "-t", "ed25519", "-N", password, "-f"])
                    .arg(path(name));
                assert!(keygen.status().await.unwrap().success());
            }
            let read = |name: &str| std::fs::read_to_string(path(name)).unwrap();
            std::fs::write(
                path("authorized_keys"),
                read("first.pub") + &read("second.pub"),
            )
            .unwrap();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            let id = tokio::process::Command::new("id")
                .arg("-un")
                .output()
                .await
                .unwrap();
            let user = String::from_utf8(id.stdout).unwrap().trim().to_owned();
            let config = format!(
                "ListenAddress 127.0.0.1\nPort {port}\nHostKey {0}/host\nAuthorizedKeysFile {0}/authorized_keys\nPidFile {0}/pid\nStrictModes no\nUsePAM no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nPermitRootLogin yes\nAllowUsers {user}\nAllowTcpForwarding yes\nAllowAgentForwarding yes\nAcceptEnv *\nSetEnv HOME={0} SKYHOOK_TEST_REMOTE_ENV=remote-value\nLogLevel ERROR\n",
                directory.path().display()
            );
            std::fs::write(path("sshd_config"), config).unwrap();
            let mut child = tokio::process::Command::new("/usr/bin/sshd")
                .args(["-D", "-e", "-f"])
                .arg(path("sshd_config"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            for _ in 0..100 {
                if tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .is_ok()
                {
                    return Self {
                        child,
                        directory,
                        port,
                        user,
                    };
                }
                assert!(child.try_wait().unwrap().is_none(), "fixture sshd failed");
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            panic!("fixture SSH server timed out")
        }

        async fn target(&self, name: &str, key: &str) -> TargetDefinition {
            let ssh = SshOptions {
                user: Some(self.user.clone()),
                port: Some(self.port),
                auth: TargetAuth::Key {
                    path: self.directory.path().join(key),
                },
            };
            let config = TargetConfig {
                r#type: TargetConfigType::Ssh,
                host: "127.0.0.1".into(),
                workspace: self.directory.path().into(),
                via: None,
                ssh,
            };
            let definition =
                TargetDefinition::from_config(name.into(), config, TargetSource::Config).unwrap();
            let resolver = Arc::new(crate::target::normalize::LocalResolver);
            let normalized =
                crate::target::normalize::normalize(vec![definition], vec![], resolver);
            let mut definition = normalized.await.unwrap().remove(0);
            trust_fixture(&mut definition);
            definition
        }
    }

    fn trust_fixture(target: &mut TargetDefinition) {
        let options = &mut target.resolved.as_mut().unwrap().options;
        options.insert("stricthostkeychecking".into(), vec!["no".into()]);
        options.insert("userknownhostsfile".into(), vec!["/dev/null".into()]);
    }

    #[tokio::test]
    #[ignore = "requires sshd and loopback sockets; no shim required"]
    async fn local_environment_and_dotenv_keys_never_reach_remote_processes() {
        const CHILD: &str = "SKYHOOK_TEST_REMOTE_ENV_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // Isolate ambient variables in a child test process rather than mutate the
            // environment of a multithreaded runner; .env loading sets such variables.
            let output = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "local_environment_and_dotenv_keys_never_reach_remote_processes",
                    "--ignored",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("SKYHOOK_TEST_HOST_ENV", "ambient-host-secret")
                .env("SKYHOOK_TEST_DOTENV_KEY", "invocation-dotenv-secret")
                .env("SKYHOOK_TEST_REMOTE_ENV", "incorrect-host-value")
                .kill_on_drop(true)
                .output()
                .await
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "{stdout}\n{stderr}");
            return;
        }
        let run = async {
            let server = Server::start().await;
            let mut target = server.target("environment", "first").await;
            let options = &mut target.resolved.as_mut().unwrap().options;
            // The server accepts every variable. A vulnerable client would export both
            // inherited host variables and literal SSH SetEnv keys.
            options.insert("sendenv".into(), vec!["*".into()]);
            let set_env = "SKYHOOK_TEST_CONFIG_KEY=ssh-config-secret";
            options.insert("SeTeNv".into(), vec![set_env.into()]);
            let command = "printf '%s\\n' \"${SKYHOOK_TEST_HOST_ENV-unset}\" \"${SKYHOOK_TEST_DOTENV_KEY-unset}\" \"${SKYHOOK_TEST_CONFIG_KEY-unset}\" \"${SKYHOOK_TEST_REMOTE_ENV-unset}\" \"$HOME\"";
            let prompts = Arc::new(RejectSensitivePrompts);
            let transport =
                crate::remote::ssh::open(&[target], command, &Default::default(), prompts)
                    .await
                    .unwrap();
            let Transport {
                mut input,
                mut output,
                owner: _owner,
            } = transport;
            input.shutdown().await.unwrap();
            let mut text = String::new();
            output.read_to_string(&mut text).await.unwrap();
            let home = server.directory.path().display();
            assert_eq!(text, format!("unset\nunset\nunset\nremote-value\n{home}\n"));
        };
        tokio::time::timeout(std::time::Duration::from_secs(30), run)
            .await
            .expect("SSH environment isolation test timed out");
    }

    #[tokio::test]
    #[ignore = "requires a built shim, sshd and loopback sockets; set SKYHOOK_TEST_SHIM"]
    async fn native_jumps_and_shim_owned_connections_share_a_lazy_central_agent() {
        tokio::time::timeout(std::time::Duration::from_secs(120), exercise_connections())
            .await
            .expect("SSH integration timed out");
    }

    async fn exercise_connections() {
        let server = Server::start().await;
        let workspace = server.directory.path();
        let shim = std::env::var_os("SKYHOOK_TEST_SHIM").expect("set SKYHOOK_TEST_SHIM");
        let arch = std::env::consts::ARCH;
        let assets = [(
            format!("linux-ssh-{arch}"),
            std::borrow::Cow::Owned(std::fs::read(shim).unwrap()),
        )];
        let prompts = Arc::new(Prompts(AtomicUsize::new(0)));
        let prompted = || prompts.0.load(Ordering::SeqCst);
        let catalog = EmbeddedShimCatalog::from_embedded_assets(assets).unwrap();
        let manager = RemoteManager::new(catalog, prompts.clone(), allow_all());
        let first = server.target("first", "first").await;
        let mut native = first.clone();
        native.name = "native".into();
        native.via = Some("first".into());
        let cancel = CancellationToken::new();
        let connect = |route| manager.connection(route, workspace, &cancel);
        let a = connect(route(vec![first.clone()])).await.unwrap();
        let b = connect(route(vec![first.clone(), native])).await.unwrap();
        assert_eq!(prompted(), 0);
        let nested = server.target("nested", "second").await;
        let store = crate::session::SessionStore::create_ephemeral(workspace)
            .await
            .unwrap();
        let registry = crate::target::TargetRegistry::from_definitions([first.clone()]).unwrap();
        let router = crate::target::TargetRouter::new(registry, manager.clone(), allow_all());
        let mut capabilities = crate::tool::policy::CapabilitySet::default();
        capabilities.insert(crate::tool::policy::Capability::Targets);
        let subject = crate::tool::authorization::AuthorizationSubject {
            agent: crate::identity::AgentId::root(store.id()),
            job: crate::identity::JobId::new(1).unwrap(),
            parent: None,
            scope: None,
            capabilities,
            cancellation: cancel.clone(),
        };
        let added = router.add(nested, "first".into(), &subject, &store).await;
        let mut nested = added.unwrap();
        assert_eq!(
            (&*nested.origin, nested.via.as_deref()),
            ("first", Some("first"))
        );
        assert_eq!(nested.host, "127.0.0.1");
        assert_eq!(
            prompted(),
            0,
            "registration must not decrypt the destination key"
        );
        trust_fixture(&mut nested);
        let c = connect(route(vec![first, nested])).await.unwrap();
        assert_eq!(
            prompted(),
            1,
            "encrypted remote key should be requested only when used"
        );
        let environment = manager.environment().await.unwrap();
        let mut ssh_add = tokio::process::Command::new("ssh-add");
        let identities = ssh_add.arg("-l").envs(&environment).output().await.unwrap();
        assert!(identities.status.success());
        let identities = String::from_utf8_lossy(&identities.stdout).lines().count();
        assert_eq!(identities, 2, "both keys belong to root's managed agent");
        let (_, input) = tokio::sync::mpsc::channel(1);
        let context = ToolContext::new(
            subject,
            crate::execution::ExecutionLocation::named("nested", workspace.into()),
            crate::execution::ExecutionLocation::root(workspace.into()),
            input,
            crate::job::JobManager::new(store),
        );
        let arguments = serde_json::json!({"argv":["ssh-add","-l"]});
        let listing = c
            .clone()
            .execute("exec".into(), arguments, &context)
            .await
            .unwrap();
        assert_eq!(listing.value["exit_code"], 0);
        assert_eq!(
            listing.value["stdout"].as_str().unwrap().lines().count(),
            2,
            "ordinary remote commands receive the forwarded central agent"
        );
        // Exercise duplex flow control well beyond a stream window.
        let echo = vec![server.target("echo", "first").await];
        let mut transport = c
            .connection
            .clone()
            .open_ssh(echo, "cat".into())
            .await
            .unwrap();
        let bytes = vec![b'x'; 2 * 1024 * 1024];
        let send = async {
            transport.input.write_all(&bytes).await.unwrap();
            transport.input.shutdown().await.unwrap();
        };
        let receive = async {
            let mut actual = Vec::new();
            transport.output.read_to_end(&mut actual).await.unwrap();
            assert_eq!(actual, bytes);
        };
        let duplex = async { tokio::join!(send, receive) };
        tokio::time::timeout(std::time::Duration::from_secs(20), duplex)
            .await
            .unwrap();
        drop((transport, a, b, c));
        let socket = environment["SSH_AUTH_SOCK"].clone();
        manager.shutdown().await;
        assert!(!std::path::Path::new(&socket).exists());
    }
}

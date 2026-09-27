//! Skyhook is a provider-neutral coding-agent harness whose registered tools are
//! available both as model tool calls and inside a sandboxed JavaScript runtime.

pub mod agent;
pub mod config;
pub mod execution;
pub mod fs;
pub mod identity;
pub mod job;
mod json_schema;
pub mod mcp;
pub mod media;
mod named_enum;
mod newtype;
#[cfg(unix)]
mod process_group;
pub mod provider;
pub mod remote;
pub mod session;
pub mod target;
pub mod tool;
mod yaml;

pub use {
    named_enum::UnknownName,
    newtype::{Blank, Prose},
    yaml::YamlError,
};

pub(crate) fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    media::BlobDigest::of(bytes.as_ref()).to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio_util::sync::CancellationToken;

    use crate::{
        execution::ExecutionLocation,
        identity::{AgentId, JobId},
        job::{JobLease, JobManager, JobWorker, stage},
        provider::profile::ModelProfile,
        session::SessionStore,
        tool::{
            ToolContext, ToolRegistryBuilder,
            authorization::AuthorizationSubject,
            executor::ToolExecutor,
            policy::{AllowAll, AuthorizationRequest, Policy, PolicyDecision, PolicyFuture},
        },
    };

    /// Await `future`, failing the test at the caller if it stalls.
    #[track_caller]
    pub(crate) fn bounded<T>(future: impl Future<Output = T>) -> impl Future<Output = T> {
        let caller = std::panic::Location::caller();
        async move {
            tokio::time::timeout(std::time::Duration::from_secs(20), future)
                .await
                .unwrap_or_else(|_| panic!("test synchronization timed out at {caller}"))
        }
    }

    /// Drive `operation` until `reached` completes, then fire every timer due
    /// within `limit` at once, so the deadline under test expires only after the
    /// state it guards is reached, never racing real work. No other guard may be
    /// pending meanwhile; the rest of `operation` is bounded. Current-thread only.
    pub(crate) async fn expire<T, R>(
        operation: impl Future<Output = T>,
        reached: impl Future<Output = R>,
        limit: std::time::Duration,
    ) -> (T, R) {
        let mut operation = std::pin::pin!(operation);
        let reached = tokio::select! {
            _ = &mut operation => panic!("finished before its deadline expired"),
            reached = reached => reached,
        };
        tokio::time::pause();
        tokio::time::advance(limit).await;
        tokio::time::resume();
        (bounded(operation).await, reached)
    }

    pub(crate) fn limit(tokens: u64) -> std::num::NonZeroU64 {
        std::num::NonZeroU64::new(tokens).unwrap()
    }

    /// A profile with a 128k-token context and a 4k-token output limit.
    pub(crate) fn profile(model: &str, supports_images: bool) -> ModelProfile {
        ModelProfile::new(
            model.parse().unwrap(),
            None,
            limit(128_000),
            limit(4096),
            supports_images,
        )
    }

    /// A loopback port that refuses connections: bound, never listening. It stays
    /// reserved while this lives, since a released port can be taken by a
    /// concurrent test's server, which would then receive this test's requests.
    /// On Unix a same-user process can still listen on it with `SO_REUSEPORT`.
    pub(crate) struct RefusedPort(tokio::net::TcpSocket);

    impl RefusedPort {
        pub(crate) fn new() -> Self {
            let socket = tokio::net::TcpSocket::new_v4().unwrap();
            #[cfg(unix)]
            socket.set_reuseport(true).unwrap();
            socket.bind(([127, 0, 0, 1], 0).into()).unwrap();
            Self(socket)
        }

        pub(crate) fn address(&self) -> std::net::SocketAddr {
            self.0.local_addr().unwrap()
        }
    }

    /// A valid PNG image: the PNG signature followed by `tail`.
    pub(crate) fn png(tail: &[u8]) -> crate::media::Image {
        crate::media::Image::new([&b"\x89PNG\r\n\x1a\n"[..], tail].concat()).unwrap()
    }

    /// Records every authorization request and decides synchronously from it.
    pub(crate) struct RecordingPolicy {
        pub requests: Mutex<Vec<AuthorizationRequest>>,
        pub decide: Box<dyn Fn(&AuthorizationRequest) -> PolicyDecision + Send + Sync>,
    }

    impl RecordingPolicy {
        pub fn allowing() -> Arc<Self> {
            Self::deciding(|_| PolicyDecision::allow())
        }

        pub fn deciding(
            decide: impl Fn(&AuthorizationRequest) -> PolicyDecision + Send + Sync + 'static,
        ) -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                decide: Box::new(decide),
            })
        }
    }

    impl Policy for RecordingPolicy {
        fn authorize(&self, request: AuthorizationRequest) -> PolicyFuture<'_> {
            let decision = (self.decide)(&request);
            self.requests.lock().unwrap().push(request);
            Box::pin(async move { decision })
        }
    }

    pub(crate) struct TestRuntime {
        pub root: tempfile::TempDir,
        pub store: SessionStore,
        pub agent: AgentId,
        pub jobs: JobManager,
    }

    impl TestRuntime {
        pub async fn new() -> Self {
            Self::with_durability(false).await
        }

        /// Journals under `root/sessions`, for tests that reopen the session.
        pub async fn on_disk() -> Self {
            Self::with_durability(true).await
        }

        async fn with_durability(durable: bool) -> Self {
            let root = tempfile::tempdir().unwrap();
            let sessions = root.path().join("sessions");
            let store = if durable {
                SessionStore::create(&sessions).await
            } else {
                SessionStore::create_ephemeral(&sessions).await
            }
            .unwrap();
            Self {
                agent: crate::session::tests::started(&store, root.path()).await,
                jobs: JobManager::new(store.clone()),
                store,
                root,
            }
        }

        pub fn executor(&self, builder: ToolRegistryBuilder) -> ToolExecutor {
            self.executor_with_policy(builder, Arc::new(AllowAll))
        }

        pub fn executor_with_policy(
            &self,
            builder: ToolRegistryBuilder,
            policy: Arc<dyn Policy>,
        ) -> ToolExecutor {
            ToolExecutor::new(
                builder.build(),
                policy,
                self.jobs.clone(),
                self.root.path().to_path_buf(),
            )
        }

        pub fn subject(&self, job: JobId, cancellation: CancellationToken) -> AuthorizationSubject {
            AuthorizationSubject {
                agent: self.agent.clone(),
                job,
                parent: None,
                capabilities: Default::default(),
                cancellation,
            }
        }

        /// A root-located context for a running job, and the worker that
        /// keeps it alive.
        pub fn tool_context(&self, lease: JobLease<stage::Running>) -> (ToolContext, JobWorker) {
            let location = ExecutionLocation::root(self.root.path().to_owned());
            let subject = self.subject(lease.id(), lease.cancellation_token());
            let (input, worker) = lease.split();
            let context = ToolContext::new(
                subject,
                location.clone(),
                location,
                input,
                self.jobs.clone(),
            );
            (context, worker)
        }
    }

    /// A PNG, at `image.png` in an image runtime's workspace.
    pub(crate) const IMAGE: &[u8] = b"\x89PNG\r\n\x1a\nattachment test";

    /// An executor offering the filesystem, job-control and script tools, and
    /// the slot through which its script tool reaches it.
    pub(crate) fn tool_executor(
        jobs: JobManager,
        root: &std::path::Path,
    ) -> (ToolExecutor, Arc<std::sync::OnceLock<ToolExecutor>>) {
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register_local(crate::tool::builtins::filesystem::register)
            .unwrap();
        crate::tool::builtins::jobs::register(&mut builder, jobs.clone()).unwrap();
        let slot = Arc::new(std::sync::OnceLock::new());
        crate::tool::builtins::install_script_tool(&mut builder, Arc::downgrade(&slot)).unwrap();
        let executor = ToolExecutor::new(builder.build(), Arc::new(AllowAll), jobs, root.into());
        assert!(slot.set(executor.clone()).is_ok());
        (executor, slot)
    }

    /// A runtime whose workspace holds [`IMAGE`], with its [`tool_executor`].
    pub(crate) async fn image_runtime() -> (
        TestRuntime,
        ToolExecutor,
        Arc<std::sync::OnceLock<ToolExecutor>>,
    ) {
        let runtime = TestRuntime::new().await;
        tokio::fs::write(runtime.root.path().join("image.png"), IMAGE)
            .await
            .unwrap();
        let (executor, slot) = tool_executor(runtime.jobs.clone(), runtime.root.path());
        (runtime, executor, slot)
    }

    /// Run `source` as a model's script call.
    pub(crate) async fn script(
        executor: &ToolExecutor,
        agent: &AgentId,
        source: String,
        bg: bool,
    ) -> crate::tool::executor::ExecutionResult {
        let arguments = serde_json::json!({"source":source, "bg":bg});
        executor
            .run_model(agent, crate::tool::builtins::names::SCRIPT, arguments)
            .await
            .unwrap()
    }

    /// `images` is one PNG whose bytes a model request loads as [`IMAGE`].
    pub(crate) async fn assert_loaded(store: &SessionStore, images: Vec<crate::media::ImageRef>) {
        use crate::provider::protocol::{Message, ModelRequest, ToolResult};
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].format, crate::media::ImageFormat::Png);
        let blob = images[0].blob;
        let mut request = ModelRequest {
            history: vec![Message::Tool(vec![ToolResult {
                call_id: "output".into(),
                name: "jobs".into(),
                result: serde_json::json!({}),
                images,
                is_error: false,
            }])],
            ..ModelRequest::test("image-test")
        };
        assert!(request.blobs.get(&blob).is_err());
        store
            .load_blobs(&mut request, &mut Default::default())
            .await
            .unwrap();
        assert_eq!(request.blobs.get(&blob).unwrap(), IMAGE);
    }
}

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

/// Run blocking `work` off the async workers.
pub(crate) async fn blocking<T, E>(
    work: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<T, E>
where
    T: Send + 'static,
    E: From<tokio::task::JoinError> + Send + 'static,
{
    tokio::task::spawn_blocking(work).await?
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
            policy::{AllowAll, Approval, AuthorizationRequest, Policy, PolicyDecision},
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

    /// Wait until the clock is strictly past `millis`, so a record dated now
    /// would show as later.
    pub(crate) async fn past(millis: i64) {
        bounded(async {
            while chrono::Utc::now().timestamp_millis() <= millis {
                tokio::task::yield_now().await;
            }
        })
        .await;
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
        elapse(limit).await;
        (bounded(operation).await, reached)
    }

    /// Fire every timer due within `limit` at once. Current-thread only.
    pub(crate) async fn elapse(limit: std::time::Duration) {
        tokio::time::pause();
        // Timers fire on whole milliseconds: a deadline `limit` after a moment
        // inside the current one rounds up past `limit`.
        tokio::time::advance(limit + std::time::Duration::from_millis(1)).await;
        tokio::time::resume();
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
            socket.set_reuseport(true).unwrap();
            socket.bind(([127, 0, 0, 1], 0).into()).unwrap();
            Self(socket)
        }

        pub(crate) fn address(&self) -> std::net::SocketAddr {
            self.0.local_addr().unwrap()
        }
    }

    /// A 1x1 PNG followed by `tail`, which decoding ignores: distinct tails make
    /// distinct blobs.
    pub(crate) fn png_bytes(tail: &[u8]) -> Vec<u8> {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::RgbImage::new(1, 1)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        [bytes.into_inner(), tail.to_vec()].concat()
    }

    /// Deterministic bytes that do not compress.
    pub(crate) fn noise_bytes() -> impl Iterator<Item = u8> {
        let mut seed = 0x2545_f491_u32;
        std::iter::repeat_with(move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed.to_le_bytes()
        })
        .flatten()
    }

    /// Deterministic noise encoded as `format`, so encoded sizes grow with the image.
    pub(crate) fn noise(format: image::ImageFormat, width: u32, height: u32) -> Vec<u8> {
        let pixels = noise_bytes().take(width as usize * height as usize * 4);
        let pixels = image::RgbaImage::from_vec(width, height, pixels.collect()).unwrap();
        let pixels = match format {
            image::ImageFormat::Jpeg => image::DynamicImage::from(pixels).into_rgb8().into(),
            _ => image::DynamicImage::from(pixels),
        };
        let mut bytes = std::io::Cursor::new(Vec::new());
        pixels.write_to(&mut bytes, format).unwrap();
        bytes.into_inner()
    }

    /// [`png_bytes`] as an image.
    pub(crate) async fn png(tail: &[u8]) -> crate::media::Image {
        crate::media::Image::decode(png_bytes(tail)).await.unwrap()
    }

    /// A GIF of a red frame, then a blue one.
    pub(crate) fn animated_gif() -> Vec<u8> {
        let mut animation = Vec::new();
        let frame = |color| image::Frame::new(image::RgbaImage::from_pixel(4, 2, color));
        image::codecs::gif::GifEncoder::new(&mut animation)
            .encode_frames([
                frame(image::Rgba([255, 0, 0, 255])),
                frame(image::Rgba([0, 0, 255, 255])),
            ])
            .unwrap();
        animation
    }

    /// More image attachments than one submission may carry.
    pub(crate) async fn over_limit_images() -> Vec<crate::media::Attachment> {
        let image = crate::media::Attachment::Image {
            file: None,
            image: png(b"").await,
        };
        vec![image; crate::media::MAX_IMAGES_PER_SUBMISSION + 1]
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
        fn authorize(&self, request: AuthorizationRequest) -> Approval<'_> {
            let decision = (self.decide)(&request);
            self.requests.lock().unwrap().push(request);
            Approval::Decided(decision)
        }
    }

    /// Asks for each request in turn, allowing what it proposes once answered.
    /// Dropping a request's decision withdraws it, closing its reply.
    pub(crate) struct PromptingPolicy(pub tokio::sync::mpsc::UnboundedSender<Prompt>);

    pub(crate) type Prompt = (AuthorizationRequest, tokio::sync::oneshot::Sender<()>);

    impl Policy for PromptingPolicy {
        fn authorize(&self, request: AuthorizationRequest) -> Approval<'_> {
            let grants = (request.permissions.iter())
                .filter_map(crate::tool::policy::PermissionUse::proposed_grant)
                .collect();
            Approval::Ask(Box::pin(async move {
                let (reply, replied) = tokio::sync::oneshot::channel();
                self.0.send((request, reply)).unwrap();
                match replied.await {
                    Ok(()) => PolicyDecision::Allow { grants },
                    Err(_) => PolicyDecision::Deny {
                        reason: "unanswered".into(),
                    },
                }
            }))
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

    /// The PNG at `image.png` in an image runtime's workspace.
    pub(crate) fn image_png() -> Vec<u8> {
        png_bytes(b"attachment test")
    }

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

    /// A runtime whose workspace holds [`image_png`], with its [`tool_executor`].
    pub(crate) async fn image_runtime() -> (
        TestRuntime,
        ToolExecutor,
        Arc<std::sync::OnceLock<ToolExecutor>>,
    ) {
        let runtime = TestRuntime::new().await;
        tokio::fs::write(runtime.root.path().join("image.png"), image_png())
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

    /// `images` is one PNG whose bytes a model request loads as [`image_png`].
    pub(crate) async fn assert_loaded(store: &SessionStore, images: Vec<crate::media::ImageRef>) {
        use crate::provider::{codec::common::tests::tool_result, protocol::ModelRequest};
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].format, crate::media::ImageFormat::Png);
        let blob = images[0].blob;
        let result = tool_result("output", "jobs", serde_json::json!({}), images, false);
        let mut request = ModelRequest {
            context: crate::provider::protocol::entries(vec![result]),
            ..ModelRequest::test("image-test")
        };
        assert!(request.blobs.get(&blob).is_err());
        store
            .load_blobs(&mut request, &mut Default::default())
            .await
            .unwrap();
        assert_eq!(request.blobs.get(&blob).unwrap(), image_png());
    }
}

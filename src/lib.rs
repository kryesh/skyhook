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

pub use {named_enum::UnknownName, newtype::Blank};

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
        session::SessionStore,
        tool::{
            ToolContext, ToolRegistryBuilder,
            authorization::AuthorizationSubject,
            executor::ToolExecutor,
            policy::{AllowAll, AuthorizationRequest, Policy, PolicyDecision, PolicyFuture},
        },
    };

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
}

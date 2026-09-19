//! Errors surfaced by harness construction and agent execution.

use thiserror::Error;

use crate::{
    job::JobError,
    provider::ProviderError,
    session::SessionError,
    target::TargetError,
    tool::{RegistryError, executor::ExecutionError},
};

#[derive(Debug, Error)]
pub enum HarnessError {
    #[error("workspace or configuration I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Target(#[from] TargetError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Execution(#[from] ExecutionError),
    #[error(transparent)]
    Job(#[from] JobError),
    #[error("a default model profile is required")]
    MissingDefaultModelProfile,
    #[error("unknown provider `{0}`")]
    UnknownProvider(String),
    #[error("unknown model profile `{0}`")]
    UnknownModelProfile(String),
    #[error("invalid profile: {0}")]
    InvalidProfile(String),
    #[error("compaction failed: {0}")]
    Compaction(String),
    #[error("agent stopped before completing the request")]
    AgentStopped,
    #[error("agent failed: {0}")]
    Agent(String),
    #[error("provider returned no assistant content")]
    EmptyResponse,
    /// The model declined to answer. Deterministic for a given request, so the
    /// runtime never retries it: only a parent agent or a human may retry,
    /// optionally on another model.
    #[error("the model declined to respond: {0}")]
    Refused(String),
    #[error("provider aborted response")]
    ProviderAborted,
    #[error("agent turn was interrupted")]
    Interrupted,
    #[error("child-agent nesting limit reached")]
    ChildDepth,
    #[error("image limits were exceeded")]
    ImageLimit,
    #[error("model `{0}` does not support image inputs")]
    ImagesUnsupported(String),
    #[error("harness initialization failed: {0}")]
    Initialization(String),
}

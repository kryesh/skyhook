use std::{future::Future, pin::Pin, sync::Arc};

use serde_json::Value;
use thiserror::Error;
use tokio::sync::{Mutex, mpsc};

use crate::{
    execution::ExecutionLocation,
    identity::{AgentId, JobId},
    media::ImageReference,
    tool::policy::CapabilitySet,
};

pub type ProgressFuture = Pin<Box<dyn Future<Output = Result<(), ToolError>> + Send>>;

pub trait ProgressSink: Send + Sync {
    fn publish(&self, kind: String, data: Value) -> ProgressFuture;
}

#[derive(Clone)]
pub struct ToolContext {
    pub agent: AgentId,
    pub job: JobId,
    pub execution_location: ExecutionLocation,
    pub caller_location: ExecutionLocation,
    pub capabilities: CapabilitySet,
    pub(crate) authorization: super::authorization::AuthorizationSubject,
    input: Arc<Mutex<mpsc::Receiver<Value>>>,
    progress: Arc<dyn ProgressSink>,
}

impl ToolContext {
    pub(crate) fn new(
        authorization: super::authorization::AuthorizationSubject,
        execution_location: ExecutionLocation,
        caller_location: ExecutionLocation,
        input: mpsc::Receiver<Value>,
        progress: Arc<dyn ProgressSink>,
    ) -> Self {
        Self {
            agent: authorization.agent.clone(),
            job: authorization.job,
            execution_location,
            caller_location,
            capabilities: authorization.capabilities.clone(),
            authorization,
            input: Arc::new(Mutex::new(input)),
            progress,
        }
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.authorization.cancellation.is_cancelled()
    }

    /// Wait until cancellation is requested for this tool invocation.
    pub async fn cancelled(&self) {
        self.authorization.cancellation.cancelled().await;
    }

    pub async fn receive(&self) -> Result<Value, ToolError> {
        let mut input = self.input.lock().await;
        tokio::select! {
            received = input.recv() => received.ok_or(ToolError::InputClosed),
            () = self.cancelled() => Err(ToolError::Cancelled),
        }
    }

    pub async fn progress(&self, kind: impl Into<String>, data: Value) -> Result<(), ToolError> {
        self.progress.publish(kind.into(), data).await
    }
}

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct ToolOutput {
    pub value: Value,
    pub images: Vec<ImageReference>,
}

impl ToolOutput {
    #[must_use]
    pub const fn new(value: Value) -> Self {
        Self {
            value,
            images: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_images(mut self, images: Vec<ImageReference>) -> Self {
        self.images = images;
        self
    }
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool arguments must be a JSON object")]
    ArgumentsMustBeObject,
    #[error("`bg` must be a boolean")]
    InvalidBackground,
    #[error("tool `{0}` does not support background execution")]
    BackgroundUnsupported(String),
    #[error("invalid tool arguments: {0}")]
    InvalidArguments(String),
    #[error("tool input channel is closed")]
    InputClosed,
    #[error("tool was cancelled")]
    Cancelled,
    #[error("tool failed: {0}")]
    Failed(String),
    #[error("tool failed: {message}")]
    FailedWithOutput { message: String, output: ToolOutput },
    #[error("tool I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("tool JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

impl ToolError {
    pub(crate) fn concise_message(&self) -> String {
        match self {
            Self::Failed(message) | Self::FailedWithOutput { message, .. } => message.clone(),
            _ => self.to_string(),
        }
    }

    #[must_use]
    pub fn with_output(message: impl Into<String>, output: ToolOutput) -> Self {
        Self::FailedWithOutput {
            message: message.into(),
            output,
        }
    }
}

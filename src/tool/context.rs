use std::{
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use serde_json::Value;
use thiserror::Error;
use tokio::sync::{Mutex, mpsc};

use crate::{
    identity::{AgentId, JobId},
    media::ImageReference,
};

pub type ProgressFuture = Pin<Box<dyn Future<Output = Result<(), ToolError>> + Send>>;

pub trait ProgressSink: Send + Sync {
    fn publish(&self, kind: String, data: Value) -> ProgressFuture;
}

#[derive(Clone)]
pub struct ToolContext {
    pub agent: AgentId,
    pub job: JobId,
    pub workspace: PathBuf,
    cancelled: Arc<AtomicBool>,
    input: Arc<Mutex<mpsc::Receiver<Value>>>,
    progress: Arc<dyn ProgressSink>,
}

impl ToolContext {
    pub(crate) fn new(
        agent: AgentId,
        job: JobId,
        workspace: PathBuf,
        cancelled: Arc<AtomicBool>,
        input: mpsc::Receiver<Value>,
        progress: Arc<dyn ProgressSink>,
    ) -> Self {
        Self {
            agent,
            job,
            workspace,
            cancelled,
            input: Arc::new(Mutex::new(input)),
            progress,
        }
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    pub(crate) fn cancellation(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }

    pub async fn receive(&self) -> Result<Value, ToolError> {
        loop {
            if self.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            let received = {
                let mut input = self.input.lock().await;
                tokio::time::timeout(std::time::Duration::from_millis(25), input.recv()).await
            };
            match received {
                Ok(Some(value)) => return Ok(value),
                Ok(None) => return Err(ToolError::InputClosed),
                Err(_) => {}
            }
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
    #[must_use]
    pub fn with_output(message: impl Into<String>, output: ToolOutput) -> Self {
        Self::FailedWithOutput {
            message: message.into(),
            output,
        }
    }
}

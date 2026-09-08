use std::sync::Arc;

use serde_json::Value;
use thiserror::Error;
use tokio::sync::{Mutex, mpsc};

use crate::{
    execution::ExecutionLocation,
    identity::{AgentId, JobId},
    media::ImageReference,
    tool::policy::CapabilitySet,
};

#[derive(Clone)]
pub struct ToolContext {
    pub agent: AgentId,
    pub job: JobId,
    pub execution_location: ExecutionLocation,
    pub caller_location: ExecutionLocation,
    pub capabilities: CapabilitySet,
    pub(crate) process_environment: crate::remote::authentication::ProcessEnvironment,
    pub(crate) authorization: super::authorization::AuthorizationSubject,
    input: Arc<Mutex<mpsc::Receiver<Value>>>,
    jobs: crate::job::JobManager,
}

impl ToolContext {
    pub(crate) fn new(
        authorization: super::authorization::AuthorizationSubject,
        execution_location: ExecutionLocation,
        caller_location: ExecutionLocation,
        input: mpsc::Receiver<Value>,
        jobs: crate::job::JobManager,
    ) -> Self {
        Self {
            agent: authorization.agent.clone(),
            job: authorization.job,
            execution_location,
            caller_location,
            capabilities: authorization.capabilities.clone(),
            process_environment: Default::default(),
            authorization,
            input: Arc::new(Mutex::new(input)),
            jobs,
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

    pub(crate) async fn drain_input_or_close(&self) -> Vec<Value> {
        let mut input = self.input.lock().await;
        let operation = self.jobs.operation(self.job).await.expect("live tool job");
        let _operation = operation.lock().await;
        let mut pending = Vec::new();
        while let Ok(value) = input.try_recv() {
            pending.push(value);
        }
        if pending.is_empty() {
            input.close();
            // A sender on another thread may enqueue between try_recv and
            // close. Closing rejects future sends, but preserves accepted ones.
            while let Ok(value) = input.try_recv() {
                pending.push(value);
            }
        }
        pending
    }

    pub async fn receive(&self) -> Result<Value, ToolError> {
        let mut input = self.input.lock().await;
        tokio::select! {
            received = input.recv() => received.ok_or(ToolError::InputClosed),
            () = self.cancelled() => Err(ToolError::Cancelled),
        }
    }

    pub(crate) fn cancellation_token(&self) -> crate::job::CancellationToken {
        self.authorization.cancellation.clone()
    }

    pub(crate) async fn capture_path(&self, field: &str) -> Result<std::path::PathBuf, ToolError> {
        let directory = self.jobs.output_directory(self.job);
        tokio::fs::create_dir_all(&directory).await?;
        Ok(crate::job::output::field_file(&directory, field))
    }

    pub(crate) async fn output_changed(&self) {
        self.jobs.output_changed(self.job).await;
    }
}

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct ToolOutput {
    pub value: Value,
    pub images: Vec<ImageReference>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub console_output: String,
}

impl ToolOutput {
    #[must_use]
    pub const fn new(value: Value) -> Self {
        Self {
            value,
            images: Vec::new(),
            console_output: String::new(),
        }
    }

    #[must_use]
    pub fn with_images(mut self, images: Vec<ImageReference>) -> Self {
        self.images = images;
        self
    }
}

/// Metadata for a rejected operation. The enclosing script may already have run other work.
#[derive(
    Clone, Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema, PartialEq, Eq,
)]
pub struct Denial {
    pub code: DenialCode,
    pub executed: bool,
}
#[derive(
    Clone, Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub enum DenialCode {
    PermissionDenied,
}
impl Denial {
    pub(crate) const fn permission_denied() -> Self {
        Self {
            code: DenialCode::PermissionDenied,
            executed: false,
        }
    }
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("operation denied: {0}")]
    Denied(String),
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
    FailedWithOutput {
        message: String,
        output: Box<ToolOutput>,
    },
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
            output: Box::new(output),
        }
    }
}

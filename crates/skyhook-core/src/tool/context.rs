use std::sync::Arc;

use serde_json::Value;
use thiserror::Error;
use tokio::sync::{Mutex, mpsc};

use crate::{
    execution::ExecutionLocation,
    identity::{AgentId, JobId},
    media::ImageReference,
    tool::policy::{Capability, CapabilitySet, PermissionUse, ResourceId},
};

#[derive(Clone)]
pub struct ToolContext {
    pub agent: AgentId,
    pub job: JobId,
    pub execution_location: ExecutionLocation,
    pub caller_location: ExecutionLocation,
    pub capabilities: CapabilitySet,
    pub(crate) process_environment: crate::remote::backend::ProcessEnvironment,
    pub(crate) authorization: super::authorization::AuthorizationSubject,
    pub(crate) authorizer: Option<(
        super::authorization::AuthorizationCoordinator,
        String,
        Value,
    )>,
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
            authorizer: None,
            input: Arc::new(Mutex::new(input)),
            jobs,
        }
    }

    /// Authorize an additional operation using this invocation's identity,
    /// capability ceiling, cancellation and (on workers) forwarding scope.
    async fn authorize(
        &self,
        permissions: Vec<PermissionUse>,
        arguments: Value,
    ) -> Result<(), ToolError> {
        let Some((coordinator, tool, _)) = &self.authorizer else {
            return Err(ToolError::Denied(
                "runtime authorization is unavailable".to_owned(),
            ));
        };
        coordinator
            .authorize(&self.authorization, tool.clone(), permissions, arguments)
            .await
            .map_err(|error| {
                use super::authorization::AuthorizationError;
                match error {
                    AuthorizationError::Cancelled => ToolError::Cancelled,
                    AuthorizationError::Denied(reason)
                    | AuthorizationError::InvalidGrant(reason) => ToolError::Denied(reason),
                    AuthorizationError::Unavailable => {
                        ToolError::Denied("required capability is unavailable".to_owned())
                    }
                }
            })
    }

    /// Approve a redirect destination before connecting to it. Callers must pass
    /// the normalized HTTP(S) origin from their parsed URL, never a full URL.
    /// No persistent grant is proposed; each invocation/destination is reviewed.
    pub async fn authorize_network(&self, normalized_origin: &str) -> Result<(), ToolError> {
        let mut arguments = self
            .authorizer
            .as_ref()
            .map_or(Value::Null, |(_, _, arguments)| arguments.clone());
        if let Some(object) = arguments.as_object_mut() {
            object.insert(
                "network_origin".to_owned(),
                Value::String(normalized_origin.to_owned()),
            );
        }
        self.authorize(
            vec![PermissionUse::new(
                Capability::Network,
                ResourceId::network(&self.execution_location.target, normalized_origin),
            )],
            arguments,
        )
        .await
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
        self.capture_path_with_kind(field, crate::job::output::CaptureKind::Text)
            .await
    }

    pub(crate) async fn capture_path_with_kind(
        &self,
        field: &str,
        kind: crate::job::output::CaptureKind,
    ) -> Result<std::path::PathBuf, ToolError> {
        let directory = self.jobs.output_directory(self.job);
        let field = field.to_owned();
        tokio::task::spawn_blocking(move || {
            crate::job::output::register_capture(&directory, &field, kind)
        })
        .await
        .map_err(|error| ToolError::Failed(error.to_string()))?
        .map_err(Into::into)
    }

    pub(crate) async fn output_changed(&self) {
        self.jobs.output_changed(self.job).await;
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
    #[error("tool was interrupted")]
    Interrupted,
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

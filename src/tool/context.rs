use std::sync::Arc;

use crate::tool::invocation::AdmissionError;
use serde_json::Value;
use tokio::sync::{Mutex, mpsc};

use crate::{
    execution::ExecutionLocation,
    identity::{AgentId, JobId},
    media::ImageRef,
    tool::policy::{Capability, CapabilitySet, PermissionUse, ResourceId},
};

#[derive(Clone)]
enum Authority {
    Invocation {
        coordinator: super::authorization::AuthorizationCoordinator,
        tool: String,
        arguments: Value,
    },
    UnavailableForResume,
}

#[derive(Clone)]
pub struct ToolContext {
    subject: super::authorization::AuthorizationSubject,
    execution_location: ExecutionLocation,
    caller_location: ExecutionLocation,
    pub(crate) process_environment: crate::remote::backend::ProcessEnvironment,
    authority: Authority,
    input: Arc<Mutex<mpsc::Receiver<Value>>>,
    jobs: crate::job::JobManager,
}

impl ToolContext {
    pub(crate) fn store(&self) -> &crate::session::SessionStore {
        self.jobs.store()
    }

    /// Construct a job-only context for resume operations, without invocation authority.
    pub(crate) fn new(
        subject: super::authorization::AuthorizationSubject,
        execution_location: ExecutionLocation,
        caller_location: ExecutionLocation,
        input: mpsc::Receiver<Value>,
        jobs: crate::job::JobManager,
    ) -> Self {
        Self {
            subject,
            execution_location,
            caller_location,
            process_environment: Default::default(),
            authority: Authority::UnavailableForResume,
            input: Arc::new(Mutex::new(input)),
            jobs,
        }
    }

    /// Attach the invocation authority prepared by the executor.
    #[must_use]
    pub(crate) fn with_invocation_authority(
        mut self,
        coordinator: super::authorization::AuthorizationCoordinator,
        tool: String,
        arguments: Value,
    ) -> Self {
        self.authority = Authority::Invocation {
            coordinator,
            tool,
            arguments,
        };
        self
    }

    #[must_use]
    pub fn agent(&self) -> &AgentId {
        &self.subject.agent
    }

    #[must_use]
    pub fn job(&self) -> JobId {
        self.subject.job
    }

    #[must_use]
    pub fn capabilities(&self) -> &CapabilitySet {
        &self.subject.capabilities
    }

    #[must_use]
    pub fn execution_location(&self) -> &ExecutionLocation {
        &self.execution_location
    }

    #[must_use]
    pub fn caller_location(&self) -> &ExecutionLocation {
        &self.caller_location
    }

    pub(crate) fn diagnostic_viewer(&self) -> super::diagnostic::DiagnosticViewer<'_> {
        super::diagnostic::DiagnosticViewer::new(self.capabilities(), self.caller_location())
    }

    /// Identity/provenance for legitimate resumed job contexts. This is not
    /// invocation authority; APIs requiring that must use invocation_subject.
    pub(crate) fn job_subject(&self) -> &super::authorization::AuthorizationSubject {
        &self.subject
    }

    /// Borrow the admitted invocation identity for an operation that still
    /// performs its own policy check. A resumed job is deliberately not an
    /// invocation and cannot silently acquire authorization this way.
    pub(crate) fn invocation_subject(
        &self,
    ) -> Result<&super::authorization::AuthorizationSubject, ToolError> {
        self.invocation().map(|_| &self.subject).map_err(Into::into)
    }

    /// The invocation authority's coordinator, tool name and admitted arguments.
    fn invocation(
        &self,
    ) -> Result<
        (
            &super::authorization::AuthorizationCoordinator,
            &str,
            &Value,
        ),
        AdmissionError,
    > {
        match &self.authority {
            Authority::Invocation {
                coordinator,
                tool,
                arguments,
            } => Ok((coordinator, tool, arguments)),
            Authority::UnavailableForResume => Err(AdmissionError::Denied(
                "runtime authorization is unavailable".to_owned(),
            )),
        }
    }

    /// Authorize an additional operation under this invocation's capabilities.
    pub(crate) async fn authorize(
        &self,
        permissions: Vec<PermissionUse>,
        arguments: Value,
    ) -> Result<(), AdmissionError> {
        let (coordinator, tool, _) = self.invocation()?;
        coordinator
            .authorize(&self.subject, tool.to_owned(), permissions, arguments)
            .await
            .map_err(|error| {
                use super::authorization::AuthorizationError;
                match error {
                    AuthorizationError::Cancelled => AdmissionError::Cancelled,
                    AuthorizationError::Denied(reason)
                    | AuthorizationError::InvalidGrant(reason) => AdmissionError::Denied(reason),
                    AuthorizationError::Unavailable => {
                        AdmissionError::Denied("required capability is unavailable".to_owned())
                    }
                }
            })
    }

    /// Approve a redirect destination before connecting to it. Callers must pass
    /// the normalized HTTP(S) origin from their parsed URL, never a full URL.
    /// No persistent grant is proposed; each invocation/destination is reviewed.
    pub async fn authorize_network(&self, normalized_origin: &str) -> Result<(), ToolError> {
        let mut arguments = self.invocation()?.2.clone();
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
        .map_err(Into::into)
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.subject.cancellation.is_cancelled()
    }

    /// Wait until cancellation is requested for this tool invocation.
    pub async fn cancelled(&self) {
        self.subject.cancellation.cancelled().await;
    }

    pub(crate) async fn drain_input_or_close(&self) -> Vec<Value> {
        let mut input = self.input.lock().await;
        let operation = self
            .jobs
            .operation(self.job())
            .await
            .expect("live tool job");
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
        self.subject.cancellation.clone()
    }

    /// Reserve one builtin text capture, removed if abandoned before finishing.
    pub(crate) async fn text_capture(
        &self,
        field: crate::job::output::TextCaptureField,
    ) -> Result<crate::job::output::PendingCapture, ToolError> {
        let (field, kind) = (field.pointer(), crate::job::output::CaptureKind::Text);
        self.jobs
            .pending_capture(self.job(), field, kind, true)
            .await
    }
}

/// Whether a producer's streams ran to their end; a timeout cuts them short.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StreamEnd {
    #[default]
    Finished,
    Cut,
}

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct ToolOutput {
    pub value: Value,
    pub images: Vec<ImageRef>,
    /// Producer-owned fields, consumed by the job finalizer. Wire/replay values
    /// cannot deserialize completion evidence or substitute arbitrary files.
    #[serde(skip)]
    pub(crate) captures: Vec<crate::job::output::CompletedCapture>,
    #[serde(skip)]
    pub(crate) streams: StreamEnd,
    #[serde(skip)]
    pub(crate) diagnostic: Option<crate::tool::diagnostic::Diagnostic>,
}

impl ToolOutput {
    #[must_use]
    pub const fn new(value: Value) -> Self {
        Self {
            value,
            images: Vec::new(),
            captures: Vec::new(),
            streams: StreamEnd::Finished,
            diagnostic: None,
        }
    }

    /// Register the builtin read result's error-message slot for presentation.
    pub(crate) fn with_diagnostic(
        mut self,
        diagnostic: crate::tool::diagnostic::Diagnostic,
    ) -> Self {
        self.diagnostic = Some(diagnostic);
        self
    }

    pub(crate) fn with_captures(
        mut self,
        captures: Vec<crate::job::output::CompletedCapture>,
    ) -> Self {
        self.captures = captures;
        self
    }

    pub(crate) fn take_captures(&mut self) -> Vec<crate::job::output::CompletedCapture> {
        std::mem::take(&mut self.captures)
    }

    #[must_use]
    pub fn with_images(mut self, images: Vec<ImageRef>) -> Self {
        self.images = images;
        self
    }
}

#[derive(
    Clone, Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub enum DenialCode {
    PermissionDenied,
}

pub type ToolError = crate::tool::invocation::OperationError<ToolOutput>;

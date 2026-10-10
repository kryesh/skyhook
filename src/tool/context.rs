use std::sync::Arc;

use crate::tool::{authorization::AuthorizationError, invocation::AdmissionError};
use serde_json::Value;
use tokio::sync::{Mutex, mpsc};

use crate::{
    execution::ExecutionLocation,
    identity::{AgentId, JobId},
    tool::policy::CapabilitySet,
};

#[derive(Clone)]
enum Authority {
    Invocation {
        coordinator: super::authorization::AuthorizationCoordinator,
        tool: String,
        arguments: Arc<super::authorization::AuthorizationArguments>,
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
    /// The opened source argument, when the tool has one.
    source: Option<crate::tool::source::Source>,
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
            source: None,
        }
    }

    /// Record that this invocation presented `read`, for its job's finish to
    /// acknowledge: the record of a read is the outcome of the job that read it.
    pub(crate) async fn read(&self, read: Option<crate::job::Delivery>) {
        self.jobs.read(self.job(), read).await;
    }

    #[must_use]
    pub(crate) fn with_source(mut self, source: Option<crate::tool::source::Source>) -> Self {
        self.source = source;
        self
    }

    pub(crate) fn source(&self) -> Option<&crate::tool::source::Source> {
        self.source.as_ref()
    }

    /// Attach the invocation authority prepared by the executor.
    #[must_use]
    pub(crate) fn with_invocation_authority(
        mut self,
        coordinator: super::authorization::AuthorizationCoordinator,
        tool: String,
        arguments: super::authorization::AuthorizationArguments,
    ) -> Self {
        self.authority = Authority::Invocation {
            coordinator,
            tool,
            arguments: Arc::new(arguments),
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
        match self.authority {
            Authority::Invocation { .. } => Ok(&self.subject),
            Authority::UnavailableForResume => Err(AdmissionError::from(unavailable()).into()),
        }
    }

    /// Authorize further permissions under this invocation's capabilities and
    /// authorization arguments, naming the origin a running request moved to,
    /// for a connection `admitted` at those target revisions.
    pub(crate) async fn authorize(
        &self,
        permissions: Vec<super::policy::PermissionUse>,
        network_origin: Option<String>,
        admitted: &[(crate::target::TargetName, u64)],
    ) -> Result<(), AuthorizationError> {
        let Authority::Invocation {
            coordinator,
            tool,
            arguments,
        } = &self.authority
        else {
            return Err(unavailable());
        };
        let arguments = arguments.document(network_origin.as_deref());
        (coordinator.authorize(
            &self.subject,
            tool.clone(),
            permissions,
            arguments,
            admitted,
        ))
        .await
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.subject.cancellation.is_cancelled()
    }

    /// Wait until cancellation is requested for this tool invocation.
    pub async fn cancelled(&self) {
        self.subject.cancellation.cancelled().await;
    }

    /// Take the accepted input, or close the mailbox when there is none: a core live
    /// command, so no send is accepted part-way through it.
    pub(crate) async fn drain_input_or_close(&self) -> Vec<Value> {
        let mut input = self.input.clone().lock_owned().await;
        let drain = move |_: &mut _| {
            let pending: Vec<_> = std::iter::from_fn(|| input.try_recv().ok()).collect();
            if pending.is_empty() {
                input.close();
            }
            pending
        };
        self.store().live(drain).await
    }

    pub async fn receive(&self) -> Result<Value, ToolError> {
        let mut input = self.input.lock().await;
        tokio::select! {
            received = input.recv() => received.ok_or(ToolError::input_closed()),
            () = self.cancelled() => Err(ToolError::cancelled()),
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
        self.jobs.pending_capture(self.job(), field, kind).await
    }
}

fn unavailable() -> AuthorizationError {
    AuthorizationError::Denied("runtime authorization is unavailable".to_owned())
}

/// Whether a producer's streams ran to their end; a timeout cuts them short.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StreamEnd {
    #[default]
    Finished,
    Cut,
}

/// Output the session host holds, its captures imported into the job.
pub type ToolOutput = crate::tool::output::Output<crate::job::output::CompletedCapture>;

crate::named_enum::named_enum! {
    #[derive(
        Clone, Copy, Debug, serde::Serialize, schemars::JsonSchema, PartialEq, Eq,
    )]
    pub enum DenialCode {
        PermissionDenied = "permission_denied",
    }
}

pub type ToolError = crate::tool::invocation::OperationError<ToolOutput>;

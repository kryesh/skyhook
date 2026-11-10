use std::sync::Arc;

use crate::session::SessionError;
use skyhook_base::enums::NamedEnum;
use skyhook_tools::{
    diagnostic::{Cause, FailureSite, PartialContext, PartialDiagnostic},
    error::OperationError,
};

use crate::tool::authorization::AuthorizationError;
use serde_json::Value;
use skyhook_tools::{diagnostic, error::AdmissionError, path::JobWorkspace};
use skyhook_worker::diagnostic::{Effects, Operation, Subject};
use tokio::sync::{Mutex, mpsc};

use super::target_tools::ExecutionTarget;
use crate::identity::AgentId;
use crate::tool::policy::CapabilitySet;
use skyhook_tools::target::ExecutionLocation;
use skyhook_worker::job::JobId;

/// What a context may do: an admitted invocation runs on machines and acts on its
/// caller's authority; a resumed job's context has neither.
#[derive(Clone)]
enum Authority {
    Invocation(Invoked),
    Resumed,
}

/// What an admitted invocation runs on.
#[derive(Clone)]
struct Invoked {
    /// The machine the call runs on.
    target: ExecutionTarget,
    /// The machine the call's job runs on, which creates a workspace it lacks.
    job_target: ExecutionTarget,
    /// The opened source argument, when the tool has one.
    source: Option<skyhook_tools::source::Source>,
    /// How the call reaches what a job's target holds of its saved output.
    reach: Arc<dyn crate::job::output::Reach>,
}

#[derive(Clone)]
pub struct ToolContext {
    subject: super::authorization::AuthorizationSubject,
    execution_location: ExecutionLocation,
    caller_location: ExecutionLocation,
    /// Where the job runs: the execution location, unless its input places
    /// it elsewhere, with a local workspace resolved.
    job_location: ExecutionLocation,
    job_workspace: JobWorkspace,
    pub(crate) process_environment: skyhook_worker::origin::ProcessEnvironment,
    authority: Authority,
    input: Arc<Mutex<mpsc::Receiver<Value>>>,
    jobs: crate::job::JobManager,
    fit: crate::job::output::Fit,
}

impl ToolContext {
    pub(crate) fn store(&self) -> &crate::session::SessionStore {
        self.jobs.store()
    }

    pub(crate) fn jobs(&self) -> &crate::job::JobManager {
        &self.jobs
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
            job_location: execution_location.clone(),
            job_workspace: JobWorkspace::Exists,
            execution_location,
            caller_location,
            process_environment: Default::default(),
            authority: Authority::Resumed,
            input: Arc::new(Mutex::new(input)),
            jobs,
            fit: crate::job::output::Fit::Preview,
        }
    }

    /// This context as an admitted invocation's, which runs on `target`, whose job
    /// runs on `job_target`, and which reads what another job's target holds of its
    /// saved output through `reach`.
    #[must_use]
    pub(crate) fn invoked(
        mut self,
        (target, job_target): (ExecutionTarget, ExecutionTarget),
        reach: Arc<dyn crate::job::output::Reach>,
    ) -> Self {
        self.authority = Authority::Invocation(Invoked {
            target,
            job_target,
            source: None,
            reach,
        });
        self
    }

    /// How much of a finished result this invocation's reads present.
    pub(crate) const fn fit(&self) -> crate::job::output::Fit {
        self.fit
    }

    #[must_use]
    pub(crate) const fn with_fit(mut self, fit: crate::job::output::Fit) -> Self {
        self.fit = fit;
        self
    }

    /// Record that this invocation presented `read`, for its job's finish to
    /// acknowledge: the record of a read is the outcome of the job that read it.
    pub(crate) async fn read(&self, read: Option<crate::job::Delivery>) {
        self.jobs.read(self.job(), read).await;
    }

    #[must_use]
    pub(crate) fn with_job_location(
        mut self,
        location: ExecutionLocation,
        workspace: JobWorkspace,
    ) -> Self {
        self.job_location = location;
        self.job_workspace = workspace;
        self
    }

    fn invocation(&self) -> Result<&Invoked, ToolError> {
        match &self.authority {
            Authority::Invocation(invoked) => Ok(invoked),
            Authority::Resumed => Err(AdmissionError::from(unavailable()).into()),
        }
    }

    /// The machine the call runs on.
    pub(crate) fn target(&self) -> Result<&ExecutionTarget, ToolError> {
        Ok(&self.invocation()?.target)
    }

    /// The machine the call's job runs on.
    pub(crate) fn job_target(&self) -> Result<&ExecutionTarget, ToolError> {
        Ok(&self.invocation()?.job_target)
    }

    /// This invocation, given the source argument it opened.
    #[must_use]
    pub(crate) fn with_source(mut self, opened: Option<skyhook_tools::source::Source>) -> Self {
        if let Authority::Invocation(invoked) = &mut self.authority {
            invoked.source = opened;
        }
        self
    }

    pub(crate) fn source(&self) -> Option<&skyhook_tools::source::Source> {
        (self.invocation().ok()).and_then(|invoked| invoked.source.as_ref())
    }

    pub(crate) fn reach(&self) -> Option<&dyn crate::job::output::Reach> {
        (self.invocation().ok()).map(|invoked| &*invoked.reach)
    }

    /// The tables the session's host keeps in it, when its harness has them.
    pub fn extension(&self) -> Option<&crate::session::ExtensionDb> {
        self.store().extension()
    }

    pub fn agent(&self) -> &AgentId {
        &self.subject.agent
    }

    pub fn job(&self) -> JobId {
        self.subject.job
    }

    pub fn capabilities(&self) -> &CapabilitySet {
        &self.subject.capabilities
    }

    pub fn execution_location(&self) -> &ExecutionLocation {
        &self.execution_location
    }

    pub fn caller_location(&self) -> &ExecutionLocation {
        &self.caller_location
    }

    pub(crate) fn job_location(&self) -> &ExecutionLocation {
        &self.job_location
    }

    /// Create the job's workspace its call found missing, once the call has
    /// checked what it can without changing anything.
    pub(crate) async fn create_job_workspace(
        &self,
    ) -> Result<skyhook_worker::op::Directories, ToolError> {
        match self.job_workspace {
            JobWorkspace::Exists => Ok(skyhook_worker::op::Directories::Existed),
            JobWorkspace::Missing => {
                use skyhook_worker::target::Target as _;
                let workspace = &self.job_location.workspace;
                let context = PartialContext::new(
                    Operation::CreateDirectories,
                    Subject::working_directory(workspace),
                )
                .effects(Effects::PartialChange);
                let target = self.job_target()?;
                let path = workspace.clone();
                let created = target.call(skyhook_worker::op::CreateDirs { path }).await?;
                created.map_err(ToolError::annotated(context))
            }
        }
    }

    pub(crate) fn diagnostic_viewer(&self) -> diagnostic::DiagnosticViewer<'_> {
        diagnostic::DiagnosticViewer::new(self.capabilities().target_view(), self.caller_location())
    }

    /// Identity/provenance of the context's job, resumed or invoked.
    pub(crate) fn job_subject(&self) -> &super::authorization::AuthorizationSubject {
        &self.subject
    }

    /// Borrow the admitted invocation identity for an operation that still
    /// performs its own policy check.
    pub(crate) fn invocation_subject(
        &self,
    ) -> Result<&super::authorization::AuthorizationSubject, ToolError> {
        self.invocation().map(|_| &self.subject)
    }

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
}

fn unavailable() -> AuthorizationError {
    AuthorizationError::Denied("runtime authorization is unavailable".to_owned())
}

/// Output the session host holds, its captures imported into the job.
pub type ToolOutput = skyhook_tools::output::Output<crate::job::output::CompletedCapture>;

#[derive(Clone, Copy, Debug, serde::Serialize, schemars::JsonSchema, PartialEq, Eq, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub enum DenialCode {
    PermissionDenied,
}

pub type ToolError = OperationError<ToolOutput>;
/// A session error's facts: its classification, never the detail it may hold.
impl From<&SessionError> for PartialDiagnostic {
    fn from(error: &SessionError) -> Self {
        use Cause::Message;
        use SessionError::*;
        let cause = match error {
            Io(error) => {
                let mut cause = Cause::io(error);
                cause.redact_io_detail();
                cause
            }
            Unconfirmed(_) => Message(crate::session::JOURNAL_FAILED.into()),
            JournalFailed(failure) => return failure.cause().into(),
            Refused(phase) => Message(format!("the session is {phase}")),
            Database(_) => Message("session database operation failed".into()),
            ModelRequestReplay { sequence, reason } => Message(format!(
                "cannot reconstruct model request at sequence {sequence}: {reason}"
            )),
            WriterLost => Message("session writer was lost".into()),
            NotFound(session) => Message(format!("session {session} was not found")),
            Random(_) => Message("session identifier generation failed".into()),
            AlreadyOpen(session) => Message(format!("session {session} is already open")),
            UnsupportedVersion(version) => {
                Message(format!("unsupported session version {version}"))
            }
            WrongSession => Message("event belongs to another session".into()),
            IllegalTransition { sequence } => Message(format!(
                "journal entry {sequence} changes a job's phase illegally"
            )),
            Blob { reason, .. } => Message(format!("stored blob {reason}")),
        };
        Self::new(PartialContext::default().at(FailureSite::Host), cause)
    }
}

impl<O> From<&SessionError> for OperationError<O> {
    fn from(error: &SessionError) -> Self {
        Self::from_facts(error.into(), None)
    }
}

impl<O> From<SessionError> for OperationError<O> {
    fn from(error: SessionError) -> Self {
        Self::from(&error)
    }
}

#[cfg(test)]
mod tests {
    use std::{assert_matches, io};

    use crate::tool::policy::CapabilitySet;
    use serde_json::json;
    use skyhook_worker::diagnostic::{Effects, Operation, Subject};
    use skyhook_worker::job::JobId;

    use super::*;

    #[test]
    fn session_normalization_preserves_classification_and_redacts_opaque_details() {
        use crate::{db::DbError, job::JobError};

        const SECRET: &str = "private-session-payload";
        for (error, message) in [
            (
                SessionError::Database(DbError::Corrupt(SECRET.into())),
                "database operation failed",
            ),
            (
                SessionError::Refused(crate::session::SessionPhase::Closed),
                "session is closed",
            ),
            (
                SessionError::Unconfirmed(None),
                crate::session::JOURNAL_FAILED,
            ),
        ] {
            let diagnostic = PartialDiagnostic::from(&error).resolve();
            assert_matches!(&diagnostic.cause, Cause::Message(text) if text.contains(message));
            assert_eq!(diagnostic.context.site, FailureSite::Host);
            let text = diagnostic.render(CapabilitySet::default().target_view());
            assert!(!text.contains(SECRET));
        }
        let fallback = PartialContext::new(
            Operation::WriteCapture,
            Subject::Job(JobId::new(7).unwrap()),
        )
        .effects(Effects::OutputIncomplete);
        let cases: [fn() -> io::Error; 2] = [
            || io::Error::other(SECRET),
            || io::Error::from_raw_os_error(13),
        ];
        for make in cases {
            let mut expected = Cause::io(&make());
            expected.redact_io_detail();
            for error in [
                ToolError::from(SessionError::Io(make())),
                ToolError::from(&*Arc::new(SessionError::Io(make()))),
                ToolError::from(JobError::from(SessionError::Io(make()))),
            ] {
                let (diagnostic, output) = error
                    .with_result(ToolOutput::new(json!({"partial": true})))
                    .or(fallback.clone())
                    .into_parts();
                assert_eq!(diagnostic.cause, expected);
                assert_eq!(
                    diagnostic.context,
                    fallback.clone().at(FailureSite::Host).resolve()
                );
                assert_eq!(output.unwrap().value, json!({"partial": true}));
            }
        }
    }
}

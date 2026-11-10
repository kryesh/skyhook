//! Per-operation context retained whenever the fetch stops.
use crate::builtins::atomic::left_by;
use crate::diagnostic::{PartialContext, RequestEffects};
use skyhook_worker::diagnostic::{Effects, Operation, Subject};
use skyhook_worker::op::{Directories, DownloadStep, FetchStep};

use super::CallError;

pub struct FetchProgress {
    context: PartialContext,
    /// Once a request is sent, a failure no longer proves the server did nothing.
    request: RequestEffects,
    /// The directories a download created to hold its destination remain after a failure.
    pub parents: Directories,
}

impl FetchProgress {
    pub fn new() -> Self {
        Self {
            context: PartialContext::default(),
            request: RequestEffects::Unknown,
            parents: Directories::Existed,
        }
    }

    pub fn step(&mut self, operation: Operation, subject: Subject) {
        self.context = PartialContext::new(operation, subject);
    }

    /// The fetch reached `step` where it runs.
    pub fn reach(&mut self, step: FetchStep) {
        let label = |text: &str| Subject::Label(text.into());
        let (operation, subject) = match step {
            FetchStep::Client => (Operation::Prepare, label("HTTP client")),
            FetchStep::Body => (Operation::Prepare, label("HTTP request body")),
            FetchStep::ReadUpload(path) => (Operation::Read, Subject::Path(path)),
            FetchStep::Send { method, origin } => {
                self.request = RequestEffects::MayHaveExecuted;
                (
                    Operation::Send,
                    Subject::Label(format!("HTTP {method} {origin}")),
                )
            }
            FetchStep::Redirect => (Operation::Validate, label("HTTP redirect")),
            FetchStep::Receive => (Operation::Receive, label("HTTP response body")),
            // Download staging never touches the destination before its final rename.
            FetchStep::Download { step, path } => {
                let operation = match step {
                    DownloadStep::Inspect => Operation::Inspect,
                    DownloadStep::Stage => Operation::CreateCapture,
                    DownloadStep::Receive => Operation::Receive,
                    DownloadStep::Write => Operation::WriteCapture,
                    DownloadStep::Sync => Operation::SyncFile,
                };
                let subject = Subject::Path(path);
                self.context = PartialContext::new(operation, subject).effects(Effects::Unchanged);
                return;
            }
            FetchStep::Commit(destination) => (Operation::Rename, Subject::Path(destination)),
        };
        self.step(operation, subject);
    }

    /// Fill the facts the failure site left unset from the current operation.
    pub fn failure(&self, error: CallError) -> CallError {
        left_by(
            self.parents,
            error.or(self.context.clone().request(self.request)),
        )
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::diagnostic::FailureSite;

    #[test]
    fn failures_keep_the_context_their_site_chose() {
        let mut progress = FetchProgress::new();
        progress.reach(FetchStep::Send {
            method: "GET".into(),
            origin: "https://example.org".into(),
        });
        let authorization = PartialContext::new(
            Operation::Authorize,
            Subject::Label("network permission".into()),
        )
        .at(FailureSite::Host)
        .effects(Effects::NotStarted);
        let denied = progress
            .failure(CallError::denied("policy").context(authorization.clone()))
            .diagnostic();
        // A denied redirect hop does not undo the request already sent.
        assert_eq!(
            denied.context,
            authorization
                .request(RequestEffects::MayHaveExecuted)
                .resolve()
        );
        let cancelled = progress.failure(CallError::cancelled()).diagnostic();
        assert_eq!(
            cancelled.context,
            PartialContext::new(
                Operation::Send,
                Subject::Label("HTTP GET https://example.org".into())
            )
            .request(RequestEffects::MayHaveExecuted)
            .resolve()
        );
    }
}

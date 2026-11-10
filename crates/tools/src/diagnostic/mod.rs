//! Structured tool failure facts.
//!
//! Sites remain facts until presentation: durable messages must not embed a target
//! alias that a later, less privileged reader is not allowed to discover.
use std::io;

use serde::{Deserialize, Serialize};

use crate::target::ExecutionLocation;
use skyhook_base::enums::{Kinds, NamedEnum};
use skyhook_worker::diagnostic::{Effects, Failure, Operation, PathFact, PathRole, Subject};
use skyhook_worker::job::JobId;
use skyhook_worker::op::{OsError, TargetPath};

mod fault;
mod render;

pub(crate) use fault::write_too_large;
pub use render::{DiagnosticViewer, TargetView, escape_controls};
pub(crate) use render::{quoted, safe_text};

/// What a request sent over the network, to an MCP server or to a remote
/// target may have done there, independent of the local [`Effects`].
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub enum RequestEffects {
    Unknown,
    MayHaveExecuted,
}

/// Invocation is resolved by the executor, not by the tool or renderer.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Kinds)]
#[kinds(name = SiteKind, serde(rename_all = "snake_case"))]
pub enum FailureSite {
    Invocation,
    Host,
    Execution(ExecutionLocation),
}

/// The resolved facts persistence, the wire and the renderer see.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct DiagnosticContext {
    pub operation: Operation,
    pub subject: Subject,
    pub site: FailureSite,
    pub effects: Effects,
    pub request: RequestEffects,
    pub paths: Vec<PathFact>,
}

/// The facts a failure site chose. Each boundary nearer the caller fills what
/// is still unset with [`PartialContext::or`]; nothing set is ever replaced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PartialContext {
    operation: Option<(Operation, Subject)>,
    site: Option<FailureSite>,
    effects: Option<Effects>,
    request: Option<RequestEffects>,
    paths: Vec<PathFact>,
}

impl PartialContext {
    pub fn new(operation: Operation, subject: Subject) -> Self {
        Self {
            operation: Some((operation, subject)),
            ..Self::default()
        }
    }
    #[must_use]
    pub fn path(mut self, role: PathRole, path: impl Into<TargetPath>) -> Self {
        self.paths.push(PathFact {
            role,
            path: path.into(),
        });
        self
    }
    #[must_use]
    pub fn paths(mut self, paths: Vec<PathFact>) -> Self {
        self.paths = paths;
        self
    }
    /// Name what failed, keeping the stage already chosen; the neutral stage
    /// otherwise.
    #[must_use]
    pub fn subject(mut self, subject: Subject) -> Self {
        let operation = self
            .operation
            .map_or(Operation::Execute, |(operation, _)| operation);
        self.operation = Some((operation, subject));
        self
    }
    #[must_use]
    pub fn at(mut self, site: FailureSite) -> Self {
        self.site = Some(site);
        self
    }
    #[must_use]
    pub fn effects(mut self, effects: Effects) -> Self {
        self.effects = Some(effects);
        self
    }
    #[must_use]
    pub fn request(mut self, request: RequestEffects) -> Self {
        self.request = Some(request);
        self
    }
    /// Fill the facts this context leaves unset from `fallback`.
    #[must_use]
    pub fn or(self, fallback: Self) -> Self {
        Self {
            operation: self.operation.or(fallback.operation),
            site: self.site.or(fallback.site),
            effects: self.effects.or(fallback.effects),
            request: self.request.or(fallback.request),
            paths: if self.paths.is_empty() {
                fallback.paths
            } else {
                self.paths
            },
        }
    }
    /// Unset facts resolve to the neutral spellings: a plain execution failure
    /// at the invocation whose effects are its operation's.
    pub fn resolve(self) -> DiagnosticContext {
        let (operation, subject) = self
            .operation
            .unwrap_or((Operation::Execute, Subject::None));
        DiagnosticContext {
            operation,
            subject,
            site: self.site.unwrap_or(FailureSite::Invocation),
            effects: self.effects.unwrap_or(operation.unset_effects()),
            request: self.request.unwrap_or(RequestEffects::Unknown),
            paths: self.paths,
        }
    }
}

/// A resolved context, restored from persistence or the wire, has every fact set.
impl From<DiagnosticContext> for PartialContext {
    fn from(context: DiagnosticContext) -> Self {
        Self {
            operation: Some((context.operation, context.subject)),
            site: Some(context.site),
            effects: Some(context.effects),
            request: Some(context.request),
            paths: context.paths,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Kinds)]
#[kinds(name = CauseKind, serde(rename_all = "snake_case"))]
pub enum Cause {
    Io(OsError),
    InvalidArguments(String),
    Denied(String),
    Cancelled,
    Interrupted,
    InputClosed,
    Message(String),
    /// A target could not be reached, or its connection failed under the call: what
    /// kept it, and how that reads.
    Connection {
        fault: ConnectionFault,
        detail: String,
    },
    Json,
    /// The tool needs a capability this context lacks: it is unavailable here,
    /// which is not a policy denial.
    Unavailable {
        tool: String,
    },
    UnknownTool {
        tool: String,
    },
    ScriptUnavailable {
        tool: String,
    },
    UnknownJob {
        job: JobId,
    },
    JobAlreadyTerminal {
        job: JobId,
    },
    /// The path resolved to one that is not text, given as its target's native
    /// bytes: on Windows its UTF-16 code units, little-endian.
    Unspelled {
        #[serde(with = "serde_bytes")]
        native: Vec<u8>,
    },
}

/// What kept a target from being reached.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionFault {
    /// SSH could not connect, or its stream failed.
    Ssh,
    /// WinRM could not connect, or the command it ran failed.
    Winrm,
    /// A worker and its client broke the frame contract between them.
    Protocol,
    /// No shim could be installed or started on the target.
    Deployment,
    /// The worker refused a setting: the connection's root or a call's workspace.
    Refused,
    /// The configuration no longer gives a target on the route its password.
    Password,
    /// The connection's startup did not finish.
    Startup,
}

impl Cause {
    pub fn io(error: &io::Error) -> Self {
        Self::os(error.into())
    }

    /// An OS error as its target classified it.
    pub fn os(error: OsError) -> Self {
        let detail = error.detail.map(|detail| safe_text(&detail));
        Self::Io(OsError { detail, ..error })
    }

    /// Opaque subsystem messages may contain arguments, credentials or payloads.
    /// Redaction changes only detail, never classification or the originating OS code.
    pub fn redact_io_detail(&mut self) {
        if let Self::Io(error) = self {
            error.detail = None;
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Diagnostic {
    pub context: DiagnosticContext,
    pub cause: Cause,
}

/// A failure's facts before its boundaries have filled them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartialDiagnostic {
    pub context: PartialContext,
    pub cause: Cause,
}

impl PartialDiagnostic {
    pub const fn new(context: PartialContext, cause: Cause) -> Self {
        Self { context, cause }
    }
    pub const fn is_denial(&self) -> bool {
        matches!(self.cause, Cause::Denied(_))
    }
    #[must_use]
    pub fn or(mut self, fallback: PartialContext) -> Self {
        self.context = self.context.or(fallback);
        self
    }
    pub fn resolve(self) -> Diagnostic {
        Diagnostic::new(self.context.resolve(), self.cause)
    }
}

/// A target's failure, its fault rendered and filled with what only its tool names.
impl From<Failure> for PartialDiagnostic {
    fn from(failure: Failure) -> Self {
        let (cause, named) = fault::render(failure.fault);
        let context = PartialContext {
            operation: failure.operation.or(named),
            effects: failure.effects,
            paths: failure.paths,
            ..PartialContext::default()
        };
        Self::new(context, cause)
    }
}

impl From<Diagnostic> for PartialDiagnostic {
    fn from(diagnostic: Diagnostic) -> Self {
        Self::new(diagnostic.context.into(), diagnostic.cause)
    }
}

impl Diagnostic {
    pub const fn new(context: DiagnosticContext, cause: Cause) -> Self {
        Self { context, cause }
    }
    pub const fn is_denial(&self) -> bool {
        matches!(self.cause, Cause::Denied(_))
    }
}

/// Remove an opaque I/O source message at a boundary that can contain secrets,
/// retaining its typed kind and OS code without interpreting the message.
pub fn opaque_io(error: impl std::borrow::Borrow<io::Error>) -> io::Error {
    let error = error.borrow();
    match error.raw_os_error() {
        Some(code) => io::Error::from_raw_os_error(code),
        None => error.kind().into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::OperationError;

    /// Redaction drops only an I/O failure's detail: its classification and OS code stay.
    #[test]
    fn opaque_io_redacts_detail_and_keeps_classification() {
        let cases: [fn() -> io::Error; 2] = [
            || io::Error::other("private-session-payload"),
            || io::Error::from_raw_os_error(13),
        ];
        for make in cases {
            let mut expected = Cause::io(&make());
            expected.redact_io_detail();
            assert_eq!(Cause::io(&opaque_io(make())), expected);
            let error = OperationError::<()>::io(make()).opaque_io();
            assert_eq!(error.diagnostic().cause, expected);
        }
    }
}

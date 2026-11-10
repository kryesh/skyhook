//! Why a target name, field or route is invalid.
use std::borrow::Cow;

use thiserror::Error;

use super::ConnectionKind;
use skyhook_base::enums::NamedEnum;
use skyhook_tools::{
    error::OperationError,
    target::{TargetName, TargetNameError, TargetRef},
};
use skyhook_worker::diagnostic::{Operation, Subject};

use super::TargetField;

/// The field that introduces a target reference, retained when resolution fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub enum TargetEdge {
    Origin,
    Via,
}

#[derive(Clone, Debug, Error)]
pub enum TargetError {
    #[error(transparent)]
    Name(#[from] TargetNameError),
    #[error("invalid target hostname `{0}`")]
    InvalidHost(String),
    #[error("SSH option `{0}` is reserved by Skyhook")]
    ReservedSshOption(String),
    #[error("SSH option `{0}` must be yes, ask, accept-new or no")]
    InvalidHostKeyChecking(String),
    #[error("SSH option `{0}` must be yes, ask or no")]
    InvalidHostKeyUpdates(String),
    #[error("SSH option `{0}` is set through the `{1}` field, not ssh.options")]
    DedicatedSshOption(String, TargetField),
    #[error("target `{0}` sets ProxyCommand, which cannot be combined with via")]
    ProxyCommandWithVia(TargetName),
    #[error("target `{0}` sets via to its origin; omit via to connect directly from the origin")]
    ViaIsOrigin(TargetName),
    #[error(
        "target `{target}` starts SSH from `{origin}`, but its jump `{jump}` starts from `{jump_origin}`; give them the same origin"
    )]
    OriginMismatch {
        target: TargetName,
        jump: TargetName,
        origin: TargetRef,
        jump_origin: TargetRef,
    },
    #[error("{transport}.password: {problem}")]
    InvalidPassword {
        transport: ConnectionKind,
        problem: skyhook_worker::secret::SourceProblem,
    },
    #[error("a {0} target needs its {0} settings")]
    MissingSettings(ConnectionKind),
    #[error("a {transport} target takes no {settings} settings")]
    ForeignSettings {
        transport: ConnectionKind,
        settings: ConnectionKind,
    },
    #[error(
        "target `{0}` connects with WinRM, which has no jumps; reach it from an origin instead of via"
    )]
    WinrmVia(TargetName),
    #[error(
        "target `{target}` jumps through `{jump}`, which connects with WinRM; a jump must be an SSH target"
    )]
    WinrmJump {
        target: TargetName,
        jump: TargetName,
    },
    #[error("unknown target `{0}`")]
    Unknown(TargetName),
    #[error("invalid {edge} target name")]
    InvalidReference { edge: TargetEdge },
    #[error("target `{target}` references unknown {edge} target `{reference}`")]
    UnknownReference {
        target: TargetName,
        edge: TargetEdge,
        reference: TargetName,
    },
    #[error("target route contains a cycle at `{0}`")]
    Cycle(TargetName),
    #[error("`root` cannot be used as a jump target")]
    RootCannotBeJump,
    #[error(
        "target `{target}` needs the secrets of both `{first}` and `{second}` in one SSH connection, which types only one; reach `{second}` from `{first}`, or a target after it, with `origin` instead of `via`"
    )]
    SecretsInOneConnection {
        target: TargetName,
        first: TargetName,
        second: TargetName,
    },
}

/// Agent-facing reasons never carry aliases or submitted values: a saved
/// diagnostic can later be read with fewer capabilities than its first reader.
impl<O> From<TargetError> for OperationError<O> {
    fn from(error: TargetError) -> Self {
        let argument = |name: &str| Subject::argument(name.split('.'));
        let fixed = |reason: &'static str| Some(Cow::Borrowed(reason));
        let (subject, reason) = match &error {
            TargetError::Name(_) => (argument("name"), None),
            TargetError::InvalidHost(_) => (
                argument("host"),
                fixed("hostname must be nonempty and contain no whitespace or control characters"),
            ),
            TargetError::ReservedSshOption(_) => (
                argument("ssh.options"),
                fixed("SSH option is reserved by Skyhook"),
            ),
            TargetError::InvalidHostKeyChecking(_) => (
                argument("ssh.options"),
                fixed("StrictHostKeyChecking must be yes, ask, accept-new or no"),
            ),
            TargetError::InvalidHostKeyUpdates(_) => (
                argument("ssh.options"),
                fixed("UpdateHostKeys must be yes, ask or no"),
            ),
            TargetError::DedicatedSshOption(_, field) => (
                argument("ssh.options"),
                Some(
                    format!("SSH option must be set through the {field} field, not ssh.options")
                        .into(),
                ),
            ),
            TargetError::ProxyCommandWithVia(_) => (
                argument("ssh.options"),
                fixed("ProxyCommand cannot be combined with via"),
            ),
            TargetError::ViaIsOrigin(_) => (
                argument("via"),
                fixed(
                    "via cannot be the connection origin; omit via to connect directly from the origin",
                ),
            ),
            TargetError::OriginMismatch { .. } => (
                argument("origin"),
                fixed("target and its jump must start SSH from the same origin"),
            ),
            TargetError::InvalidPassword { transport, problem } => (
                argument(&format!("{transport}.password")),
                Some(problem.to_string().into()),
            ),
            TargetError::MissingSettings(transport) => (argument(transport.as_str()), None),
            TargetError::ForeignSettings { settings, .. } => (argument(settings.as_str()), None),
            TargetError::WinrmVia(_) => (
                argument("via"),
                fixed("a WinRM target has no jumps; reach it from an origin instead of via"),
            ),
            TargetError::WinrmJump { .. } => (
                argument("via"),
                fixed("a jump must be an SSH target, not a WinRM one"),
            ),
            TargetError::Unknown(_) => (argument("target"), fixed("unknown target")),
            TargetError::InvalidReference { edge } => (argument(&edge.to_string()), None),
            TargetError::UnknownReference { edge, .. } => match edge {
                TargetEdge::Origin => (argument("origin"), fixed("unknown origin target")),
                TargetEdge::Via => (argument("via"), fixed("unknown jump target")),
            },
            TargetError::Cycle(_) => (
                Subject::Label("target route".into()),
                fixed("target route contains a cycle"),
            ),
            TargetError::RootCannotBeJump => (argument("via"), None),
            TargetError::SecretsInOneConnection { .. } => (
                Subject::Label("target route".into()),
                fixed(
                    "one SSH connection types only one password or key passphrase; reach the later target from the earlier, or a target after it, with origin instead of via",
                ),
            ),
        };
        // Variants without a payload render nothing private.
        Self::invalid_arguments(reason.unwrap_or_else(|| error.to_string().into()))
            .operation(Operation::Validate, subject)
    }
}

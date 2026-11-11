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
        "target `{second}` needs the secrets of both `{first}` and itself in one SSH connection, which types only one; reach `{second}` from `{first}`, or a target after it, with `origin` instead of `via`"
    )]
    SecretsInOneConnection {
        first: TargetName,
        second: TargetName,
    },
}

impl TargetError {
    /// The target whose setting the error refuses, where a route says: a route crosses
    /// other targets, and what it refuses of one is that target's to answer for.
    pub(crate) fn target(&self) -> Option<&TargetName> {
        match self {
            Self::OriginMismatch { target, .. }
            | Self::WinrmJump { target, .. }
            | Self::UnknownReference { target, .. }
            | Self::SecretsInOneConnection { second: target, .. }
            | Self::Cycle(target) => Some(target),
            _ => None,
        }
    }

    /// The setting the error refuses, as the keys to it from the target's own fields; none
    /// refuses the target as a whole.
    pub(crate) fn field(&self) -> Vec<&'static str> {
        let field = match self {
            Self::InvalidHost(_) => "host",
            Self::ReservedSshOption(_)
            | Self::InvalidHostKeyChecking(_)
            | Self::InvalidHostKeyUpdates(_)
            | Self::DedicatedSshOption(..)
            | Self::ProxyCommandWithVia(_) => return vec!["transport", "ssh", "options"],
            Self::ViaIsOrigin(_) | Self::WinrmVia(_) | Self::WinrmJump { .. } => "via",
            Self::RootCannotBeJump => "via",
            Self::OriginMismatch { .. } => "origin",
            Self::InvalidPassword { transport, .. } => {
                return vec!["transport", transport.as_str(), "password"];
            }
            Self::InvalidReference { edge } | Self::UnknownReference { edge, .. } => edge.as_str(),
            Self::Name(_)
            | Self::Unknown(_)
            | Self::Cycle(_)
            | Self::SecretsInOneConnection { .. } => return Vec::new(),
        };
        vec![field]
    }
}

/// Agent-facing reasons never carry aliases or submitted values: a saved
/// diagnostic can later be read with fewer capabilities than its first reader.
impl<O> From<TargetError> for OperationError<O> {
    fn from(error: TargetError) -> Self {
        let fixed = |reason: &'static str| Some(Cow::Borrowed(reason));
        let reason = match &error {
            TargetError::InvalidHost(_) => {
                fixed("hostname must be nonempty and contain no whitespace or control characters")
            }
            TargetError::ReservedSshOption(_) => fixed("SSH option is reserved by Skyhook"),
            TargetError::InvalidHostKeyChecking(_) => {
                fixed("StrictHostKeyChecking must be yes, ask, accept-new or no")
            }
            TargetError::InvalidHostKeyUpdates(_) => fixed("UpdateHostKeys must be yes, ask or no"),
            TargetError::DedicatedSshOption(_, field) => Some(
                format!("SSH option must be set through the {field} field, not ssh.options").into(),
            ),
            TargetError::ProxyCommandWithVia(_) => {
                fixed("ProxyCommand cannot be combined with via")
            }
            TargetError::ViaIsOrigin(_) => fixed(
                "via cannot be the connection origin; omit via to connect directly from the origin",
            ),
            TargetError::OriginMismatch { .. } => {
                fixed("target and its jump must start SSH from the same origin")
            }
            TargetError::InvalidPassword { problem, .. } => Some(problem.to_string().into()),
            TargetError::WinrmVia(_) => {
                fixed("a WinRM target has no jumps; reach it from an origin instead of via")
            }
            TargetError::WinrmJump { .. } => fixed("a jump must be an SSH target, not a WinRM one"),
            TargetError::Unknown(_) => fixed("unknown target"),
            TargetError::UnknownReference { edge, .. } => match edge {
                TargetEdge::Origin => fixed("unknown origin target"),
                TargetEdge::Via => fixed("unknown jump target"),
            },
            TargetError::Cycle(_) => fixed("target route contains a cycle"),
            TargetError::SecretsInOneConnection { .. } => fixed(
                "one SSH connection types only one password or key passphrase; reach the later target from the earlier, or a target after it, with origin instead of via",
            ),
            TargetError::Name(_)
            | TargetError::InvalidReference { .. }
            | TargetError::RootCannotBeJump => None,
        };
        let subject = match &error {
            TargetError::Name(_) => Subject::argument(["name"]),
            TargetError::Unknown(_) => Subject::argument(["target"]),
            TargetError::Cycle(_) | TargetError::SecretsInOneConnection { .. } => {
                Subject::Label("target route".into())
            }
            error => Subject::argument(error.field()),
        };
        // Variants without a payload render nothing private.
        Self::invalid_arguments(reason.unwrap_or_else(|| error.to_string().into()))
            .operation(Operation::Validate, subject)
    }
}

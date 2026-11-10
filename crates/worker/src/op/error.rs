//! The failure vocabulary every operation shares: OS errors as the target
//! classified them, and the guards a file passes before it is opened.
use std::io;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook_base::enums::NamedEnum;

/// An OS error as the target classified it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct OsError {
    pub kind: IoKind,
    /// The target's own error code.
    pub code: Option<i32>,
    /// What a subsystem wrapped in the error, without a native message.
    pub detail: Option<String>,
}

/// A standard error made of one a target classified is that one.
impl From<&io::Error> for OsError {
    fn from(error: &io::Error) -> Self {
        let wrapped = error.get_ref();
        if let Some(classified) = wrapped.and_then(|wrapped| wrapped.downcast_ref::<Self>()) {
            return classified.clone();
        }
        Self {
            kind: error.kind().into(),
            code: error.raw_os_error(),
            detail: wrapped.map(|reason| reason.to_string()),
        }
    }
}

impl From<io::Error> for OsError {
    fn from(error: io::Error) -> Self {
        Self::from(&error)
    }
}

/// Why a regular file could not be opened, read or removed.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, thiserror::Error)]
pub enum FsError {
    #[error("{0}")]
    Os(OsError),
    #[error("not a regular file")]
    NotRegular,
    #[error("file exceeds the {limit}-byte limit")]
    TooLarge { limit: u64 },
    #[error(transparent)]
    Live(#[from] crate::fs::LiveDatabase),
}

impl From<io::Error> for FsError {
    fn from(error: io::Error) -> Self {
        Self::Os(error.into())
    }
}

/// The OS error kinds a target classifies: each is the standard kind it names, or
/// the one after `=`, and the rest are `Other`.
macro_rules! io_kinds {
    (@io $kind:ident) => { io::ErrorKind::$kind };
    (@io $kind:ident $io:ident) => { io::ErrorKind::$io };
    ($($kind:ident $(= $io:ident)?),* $(,)?) => {
        #[derive(Clone, Copy, Debug, Serialize, JsonSchema, PartialEq, Eq, NamedEnum)]
        #[serde(rename_all = "snake_case")]
        pub enum IoKind {
            $($kind,)*
            Other,
        }

        impl From<io::ErrorKind> for IoKind {
            fn from(kind: io::ErrorKind) -> Self {
                $(if kind == io_kinds!(@io $kind $($io)?) {
                    return Self::$kind;
                })*
                Self::Other
            }
        }

        impl IoKind {
            /// The standard error kind this one classifies.
            pub const fn io(self) -> io::ErrorKind {
                match self {
                    $(Self::$kind => io_kinds!(@io $kind $($io)?),)*
                    Self::Other => io::ErrorKind::Other,
                }
            }
        }
    };
}

io_kinds! {
    NotFound,
    PermissionDenied,
    AlreadyExists,
    InvalidInput,
    InvalidData,
    TimedOut,
    Interrupted,
    UnexpectedEof,
    BrokenPipe,
    ConnectionRefused,
    ConnectionReset,
    ConnectionAborted,
    NotConnected,
    WouldBlock,
    HostUnreachable,
    NetworkUnreachable,
    NetworkDown,
    AddressInUse = AddrInUse,
    AddressNotAvailable = AddrNotAvailable,
    WriteZero,
    Unsupported,
    OutOfMemory,
    IsADirectory,
    NotADirectory,
    DirectoryNotEmpty,
    ReadOnlyFilesystem,
    StorageFull,
}

/// A standard error of the kind a target reported, carrying the error itself: its
/// code is its target's system's, and means nothing to this machine's.
impl From<&OsError> for io::Error {
    fn from(error: &OsError) -> Self {
        io::Error::new(error.kind.io(), error.clone())
    }
}

impl std::error::Error for OsError {}

/// The error by the kind its target classified it as, with its code there, or as
/// what was wrapped in it.
impl std::fmt::Display for OsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = self.kind.as_str().replace('_', " ");
        match (self.code, &self.detail) {
            (Some(code), _) => write!(f, "{kind} (OS error {code})"),
            (None, Some(detail)) => f.write_str(detail),
            (None, None) => f.write_str(&kind),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An error read through a byte stream is the one its target reported, whatever
    /// this machine's system makes of the code.
    #[test]
    fn an_error_carried_as_a_standard_one_keeps_its_targets_classification() {
        // Windows' ERROR_ACCESS_DENIED is unix's EIO.
        let reported = OsError {
            kind: IoKind::PermissionDenied,
            code: Some(5),
            detail: None,
        };
        let carried = io::Error::from(&reported);
        assert_eq!(carried.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(OsError::from(carried), reported);
    }
}

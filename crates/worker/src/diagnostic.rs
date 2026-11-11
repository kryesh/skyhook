//! The facts a failure carries: what failed, on what, what it left behind, and why.
//! Operations fail with them as typed values; the host renders their text.
use std::{io, time::Duration};

use serde::{Deserialize, Serialize};
use skyhook_base::enums::{Kinds, NamedEnum};

use crate::fs::{AtomicWriteError, AtomicWriteStage};
use crate::job::JobId;
use crate::op::{Directories, FsError, OsError, Pipe, Protected, TargetPath};

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Execute,
    Validate,
    Authorize,
    Connect,
    Inspect,
    Resolve,
    Read,
    ReadDirectory,
    Create,
    CreateDirectories,
    Write,
    Remove,
    Rename,
    SetPermissions,
    SyncFile,
    OpenDirectory,
    SyncDirectory,
    Capture,
    CreateCapture,
    WriteCapture,
    FinishCapture,
    StoreImage,
    Prepare,
    Spawn,
    Wait,
    Terminate,
    Deserialize,
    Lookup,
    Load,
    Save,
    Send,
    Receive,
}

impl Operation {
    /// The effects of a failure whose site names none: an operation that only
    /// observes has no destination to report on.
    pub const fn unset_effects(self) -> Effects {
        match self {
            Self::Inspect | Self::Lookup | Self::Wait => Effects::ReadOnly,
            _ => Effects::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub enum Effects {
    Unknown,
    NotStarted,
    Unchanged,
    ReadOnly,
    DestinationReplaced,
    PartialChange,
    OutputIncomplete,
}

/// An explicitly selected safe subject, never arbitrary serialized arguments.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Kinds)]
#[kinds(name = SubjectKind, serde(rename_all = "snake_case"))]
pub enum Subject {
    None,
    Path(TargetPath),
    WorkingDirectory(TargetPath),
    StagingFile(TargetPath),
    ParentDirectory(TargetPath),
    DirectoryEntry(TargetPath),
    Argument(String),
    Tool(String),
    Job(JobId),
    Process,
    Label(String),
}

impl Subject {
    pub fn path(path: impl Into<TargetPath>) -> Self {
        Self::Path(path.into())
    }
    pub fn working_directory(path: impl Into<TargetPath>) -> Self {
        Self::WorkingDirectory(path.into())
    }
    /// The path this subject names, when it names one.
    pub fn target_path(&self) -> Option<&TargetPath> {
        match self {
            Self::Path(path)
            | Self::WorkingDirectory(path)
            | Self::StagingFile(path)
            | Self::ParentDirectory(path)
            | Self::DirectoryEntry(path) => Some(path),
            Self::None
            | Self::Argument(_)
            | Self::Tool(_)
            | Self::Job(_)
            | Self::Process
            | Self::Label(_) => None,
        }
    }
    /// A JSON pointer into the tool arguments: the spelling schema validation reports.
    pub fn argument<S: AsRef<str>>(segments: impl IntoIterator<Item = S>) -> Self {
        Self::Argument(
            segments
                .into_iter()
                .map(|segment| format!("/{}", escape_pointer_segment(segment.as_ref())))
                .collect(),
        )
    }
}

/// `segment` as one JSON pointer segment.
pub fn escape_pointer_segment(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub enum PathRole {
    Requested,
    Resolved,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PathFact {
    pub role: PathRole,
    pub path: TargetPath,
}

/// How an operation failed: the facts it knows where it failed, and why.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Failure {
    /// What failed, and on what; unset when only the tool that asked can name it.
    pub operation: Option<(Operation, Subject)>,
    pub effects: Option<Effects>,
    pub paths: Vec<PathFact>,
    pub fault: Fault,
}

/// Why an operation failed, as typed facts.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Fault {
    Os(OsError),
    NotRegular,
    TooLarge {
        limit: u64,
    },
    Live,
    /// The path resolved to one that is not text, given as its target's native
    /// bytes: on Windows its UTF-16 code units, little-endian.
    Unspelled {
        #[serde(with = "serde_bytes")]
        native: Vec<u8>,
    },
    EmptyPath,
    Protected(Protected),
    /// The file is not UTF-8: as [`std::str::Utf8Error`] describes it.
    NotText {
        valid_up_to: usize,
        error_len: Option<u8>,
    },
    CountMismatch {
        expected: usize,
        found: usize,
        /// The starting lines of the first matches, one more than are shown.
        lines: Vec<usize>,
    },
    /// The edited file would exceed `limit` bytes.
    EditTooLarge {
        bytes: usize,
        limit: usize,
    },
    /// Neither a file, a link nor a directory.
    UnsupportedEntry,
    /// A directory with entries, removed only recursively.
    NotEmpty(OsError),
    Regex,
    /// A search's glob at this index, or the set together.
    Globs(Option<usize>),
    GlobPattern,
    /// The search root is neither a file nor a directory.
    SearchRoot,
    /// The glob root is not a directory.
    GlobRoot,
    Loop,
    Traversal,
    NotDirectory,
    /// A spill has nowhere to go: its user has no cache directory.
    NoCacheDirectory,
    /// A spill directory that is not its user's alone.
    NotPrivate,
    /// A spill directory whose path is not text.
    NotUnicode,
    /// A process's pipe was missing, or reading it failed.
    Pipe {
        pipe: Pipe,
        error: Option<OsError>,
    },
    InvalidProxy,
    Client,
    Tls,
    ConnectTimeout,
    RequestTimeout,
    Connect,
    Decode,
    Body,
    Request,
    UploadNotRegular,
    TooManyRedirects,
    InvalidLocation,
    InvalidRedirectUrl,
    UnsupportedRedirectUrl,
    Downgrade,
    ResponseTooLarge,
    SaveExists,
    SaveNotFile,
    /// The whole exchange's deadline elapsed.
    Timeout(Duration),
}

impl Failure {
    /// `fault` at `operation` on `subject`.
    pub fn at(operation: Operation, subject: Subject, fault: impl Into<Fault>) -> Self {
        Self {
            operation: Some((operation, subject)),
            ..Self::from(fault.into())
        }
    }

    /// `fault` at `operation` on `path`, which it left unchanged.
    pub fn unchanged(operation: Operation, path: &TargetPath, fault: impl Into<Fault>) -> Self {
        Self::at(operation, Subject::path(path), fault).effects(Effects::Unchanged)
    }

    #[must_use]
    pub fn effects(mut self, effects: Effects) -> Self {
        self.effects = Some(effects);
        self
    }

    /// The missing parent `directory` could not be created; those above it may have been.
    pub fn parents(directory: TargetPath, error: OsError) -> Self {
        let subject = Subject::ParentDirectory(directory);
        Self::at(Operation::CreateDirectories, subject, Fault::Os(error))
            .effects(Effects::PartialChange)
    }

    /// A failed atomic replacement of `path`: only a completed rename proves it was
    /// replaced, and a commit proves nothing.
    pub fn atomic(path: &TargetPath, error: AtomicWriteError) -> Self {
        use AtomicWriteStage as Stage;
        let target = || Subject::Path(path.clone());
        let staging = || Subject::StagingFile(path.clone());
        let (operation, subject, effects) = match error.stage {
            Stage::Prepare => (Operation::Prepare, target(), Some(Effects::Unchanged)),
            Stage::InspectDestination => (Operation::Inspect, target(), Some(Effects::Unchanged)),
            Stage::CreateStaging => (Operation::Create, staging(), Some(Effects::Unchanged)),
            Stage::WriteStaging => (Operation::Write, staging(), Some(Effects::Unchanged)),
            Stage::SetPermissions => (
                Operation::SetPermissions,
                staging(),
                Some(Effects::Unchanged),
            ),
            Stage::SyncStaging => (Operation::SyncFile, staging(), Some(Effects::Unchanged)),
            Stage::Commit => (Operation::Rename, target(), None),
            Stage::OpenDirectory(directory) => (
                Operation::OpenDirectory,
                Subject::ParentDirectory(directory),
                Some(Effects::DestinationReplaced),
            ),
            Stage::SyncDirectory(directory) => (
                Operation::SyncDirectory,
                Subject::ParentDirectory(directory),
                Some(Effects::DestinationReplaced),
            ),
        };
        let mut failure = Self::at(operation, subject, Fault::Os(error.error));
        failure.effects = effects;
        if effects == Some(Effects::DestinationReplaced) {
            failure.paths.push(PathFact {
                role: PathRole::Resolved,
                path: path.clone(),
            });
        }
        failure
    }

    /// This failure as it leaves the directories `created` to hold what it makes.
    #[must_use]
    pub fn left_by(mut self, created: Directories) -> Self {
        self.effects = created.left(self.effects).or(self.effects);
        self
    }
}

impl Directories {
    /// What a failure with `effects` changed once these directories remain, when that is
    /// more: created ones are a partial change unless the destination was replaced anyway.
    pub fn left(self, effects: Option<Effects>) -> Option<Effects> {
        let replaced = effects == Some(Effects::DestinationReplaced);
        (self == Self::Created && !replaced).then_some(Effects::PartialChange)
    }
}

impl From<Fault> for Failure {
    fn from(fault: Fault) -> Self {
        Self {
            operation: None,
            effects: None,
            paths: Vec::new(),
            fault,
        }
    }
}

impl From<OsError> for Fault {
    fn from(error: OsError) -> Self {
        Self::Os(error)
    }
}

impl From<io::Error> for Fault {
    fn from(error: io::Error) -> Self {
        Self::Os(error.into())
    }
}

impl From<FsError> for Fault {
    fn from(error: FsError) -> Self {
        match error {
            FsError::Os(error) => Self::Os(error),
            FsError::NotRegular => Self::NotRegular,
            FsError::TooLarge { limit } => Self::TooLarge { limit },
            FsError::Live(_) => Self::Live,
        }
    }
}

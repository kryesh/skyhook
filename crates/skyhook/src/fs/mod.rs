//! Filesystem primitives: regular-file reads, descriptor-confined directories, entry kinds,
//! and atomic replacement.

mod atomic;
mod beneath;
mod guarded;

use std::{fs, io};

pub use atomic::{AtomicWriteError, CommitMode, PermissionPolicy, StagedFile};
pub(crate) use atomic::{AtomicWriteStage, Contents, atomic_write};
pub use beneath::{BeneathError, Directory, open_beneath_blocking, staging_name};
pub use guarded::{OpenFile, read_regular};
pub(crate) use guarded::{
    admit_regular, open_regular, open_regular_blocking, read_to_limit, remove_file_unless_live,
    remove_tree_unless_live,
};

/// Why a regular file could not be opened or read.
#[derive(Debug, thiserror::Error)]
pub enum RegularFileError {
    #[error("not a regular file")]
    NotRegular,
    #[error("file exceeds the {limit}-byte limit")]
    TooLarge { limit: u64 },
    #[error(transparent)]
    Live(#[from] LiveDatabase),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// A file of a database this process holds open. Closing a descriptor of it would release
/// SQLite's locks, so nothing but its own connections opens it.
#[derive(Debug, thiserror::Error)]
#[error(
    "this is a database this process holds open; opening it here would release its locks, so read it through the tools that own it"
)]
pub struct LiveDatabase;

/// A filesystem entry's own kind; a symlink is never followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileKind {
    File,
    Directory,
    Symlink,
    Other,
}

impl From<fs::FileType> for FileKind {
    fn from(kind: fs::FileType) -> Self {
        if kind.is_symlink() {
            Self::Symlink
        } else if kind.is_dir() {
            Self::Directory
        } else if kind.is_file() {
            Self::File
        } else {
            Self::Other
        }
    }
}

//! Filesystem primitives: canonical paths, regular-file reads, entry kinds, and atomic
//! replacement.

mod atomic;
mod guarded;
pub mod live;
pub mod spill;

use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub use atomic::AtomicWriteStage;
pub use atomic::{AtomicWriteError, CommitMode, PermissionPolicy, StagedFile, sync_directory};
pub(crate) use atomic::{Contents, StagedWriter, atomic_write};
pub use guarded::{OpenFile, open_regular_blocking, read_regular};
pub use guarded::{admit_regular, open_regular, read_to_limit};
pub(crate) use guarded::{remove_file_unless_live, remove_tree_unless_live};

/// `path` with every link resolved, as [`fs::canonicalize`] finds it, spelled as
/// Windows programs spell it.
pub fn canonical(path: &Path) -> io::Result<PathBuf> {
    fs::canonicalize(path).map(plain)
}

/// `path` as Windows programs spell it: a verbatim (`\\?\`) drive or UNC path, as
/// Windows canonicalizes and links name them, without its prefix whenever ordinary
/// parsing reads the plain spelling as the same path. Other paths are unchanged.
pub(crate) fn plain(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    if let Some(plain) = unverbatim(&path) {
        return plain;
    }
    path
}

#[cfg(windows)]
fn unverbatim(path: &Path) -> Option<PathBuf> {
    use std::path::{Component, Prefix};
    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return None;
    };
    let mut plain = PathBuf::from(match prefix.kind() {
        Prefix::VerbatimDisk(drive) => format!("{}:\\", char::from(drive)).into(),
        Prefix::VerbatimUNC(server, share) => {
            let mut root = std::ffi::OsString::from(r"\\");
            for part in [
                server,
                std::ffi::OsStr::new(r"\"),
                share,
                std::ffi::OsStr::new(r"\"),
            ] {
                root.push(part);
            }
            root
        }
        _ => return None,
    });
    for component in components {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => plain.push(name),
            _ => return None,
        }
    }
    // Ordinary parsing drops trailing dots and spaces, reads `/` as a separator and
    // device names such as `NUL` anywhere: the plain spelling must survive it unchanged.
    (std::path::absolute(&plain).ok()?.as_os_str() == plain.as_os_str()).then_some(plain)
}

/// A file of a database this process holds open. Closing a descriptor of it would release
/// SQLite's locks, so nothing but its own connections opens it.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, thiserror::Error, serde::Deserialize, serde::Serialize,
)]
#[error(
    "this is a database this process holds open; opening it here would release its locks, so read it through the tools that own it"
)]
pub struct LiveDatabase;

/// A filesystem entry's own kind; a symlink is never followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
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

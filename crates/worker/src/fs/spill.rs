//! Where a machine keeps output too large for a session to hold: one private
//! directory per session under the user's cache directory, beside the shims. The user
//! clears the directory.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::diagnostic::Fault;
use crate::op::{OsError, TargetPath};

/// A spill file as the host names it: a session's directory and a file in it, neither
/// of which can name anything outside the spill directory.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(try_from = "(String, String)", into = "(String, String)")]
pub struct SpillName {
    session: String,
    file: String,
}

/// A spill name with a part that is not one plain file name.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("a spill name's parts are letters, digits, '.', '_' and '-'")]
pub struct InvalidSpillName;

impl SpillName {
    /// `stem` with a token no other file has, in `session`'s directory: restarted
    /// jobs reuse their ids.
    pub fn unique(session: &impl std::fmt::Display, stem: &str) -> Self {
        let token = uuid::Uuid::new_v4().simple();
        Self::try_from((session.to_string(), format!("{stem}-{token}")))
            .expect("session ids and stems are plain names")
    }
}

impl TryFrom<(String, String)> for SpillName {
    type Error = InvalidSpillName;

    fn try_from((session, file): (String, String)) -> Result<Self, InvalidSpillName> {
        let plain = |part: &str| {
            let allowed = |byte: u8| byte.is_ascii_alphanumeric() || b"._-".contains(&byte);
            part.bytes().any(|byte| byte != b'.') && part.bytes().all(allowed)
        };
        match plain(&session) && plain(&file) {
            true => Ok(Self { session, file }),
            false => Err(InvalidSpillName),
        }
    }
}

impl From<SpillName> for (String, String) {
    fn from(name: SpillName) -> Self {
        (name.session, name.file)
    }
}

/// This machine's spill directory, when its user has a cache directory:
/// `$XDG_CACHE_HOME/skyhook/spill`, else `~/.cache/skyhook/spill`; on Windows
/// `%LOCALAPPDATA%\skyhook\spill`, whose inherited access is the user's alone.
pub fn root() -> Option<PathBuf> {
    let set = |name| std::env::var_os(name).filter(|value| !value.is_empty());
    let cache = match cfg!(windows) {
        true => PathBuf::from(set("LOCALAPPDATA")?),
        false => match set("XDG_CACHE_HOME") {
            Some(cache) => PathBuf::from(cache),
            None => std::env::home_dir()?.join(".cache"),
        },
    };
    Some(cache.join("skyhook").join("spill"))
}

/// A spill file as its machine resolved it: the path that names it there, and that
/// path's components as only that machine splits them.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SpillFile {
    pub path: TargetPath,
    pub components: Vec<String>,
}

/// Why a spill could not be kept or read back.
#[derive(Debug, thiserror::Error)]
pub enum SpillError {
    #[error("this user has no cache directory")]
    NoCacheDirectory,
    /// The session's directory exists as something others could have made or can use.
    #[error("not a directory only its user can use")]
    Shared,
    /// The session's directory resolves to a path that is not text, as a session
    /// names its files.
    #[error("this user's cache directory has a path that is not Unicode")]
    NotUnicode,
    /// The directory, or a file in it, failed.
    #[error(transparent)]
    At(io::Error),
}

impl SpillError {
    /// The spill failure `error` carries across a writer's or reader's interface.
    pub fn of(error: &io::Error) -> Option<&Self> {
        error.get_ref()?.downcast_ref()
    }

    /// Why the spill failed, as whoever reads its output is told.
    pub fn fault(&self) -> Fault {
        match self {
            Self::NoCacheDirectory => Fault::NoCacheDirectory,
            Self::Shared => Fault::NotPrivate,
            Self::NotUnicode => Fault::NotUnicode,
            Self::At(error) => Fault::Os(OsError {
                detail: None,
                ..OsError::from(error)
            }),
        }
    }
}

impl From<SpillError> for io::Error {
    fn from(error: SpillError) -> Self {
        let kind = match &error {
            SpillError::NoCacheDirectory => io::ErrorKind::NotFound,
            SpillError::Shared => io::ErrorKind::PermissionDenied,
            SpillError::NotUnicode => io::ErrorKind::InvalidFilename,
            SpillError::At(error) => error.kind(),
        };
        Self::new(kind, error)
    }
}

/// Where `name` is kept under `root`, as a later read of it resolves there, its
/// session's directory created for its user alone. A directory already there is used
/// only as this user's own private one, never a link to another or one others can
/// enter, and only where its path is text.
pub fn place(root: Option<&Path>, name: &SpillName) -> Result<SpillFile, SpillError> {
    let root = root.ok_or(SpillError::NoCacheDirectory)?;
    let directory = root.join(&name.session);
    let mut create = fs::DirBuilder::new();
    create.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut create, 0o700);
    create.create(&directory).map_err(SpillError::At)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let found = fs::symlink_metadata(&directory).map_err(SpillError::At)?;
        // SAFETY: geteuid has no preconditions and cannot fail.
        let own = found.uid() == unsafe { libc::geteuid() };
        if !(found.is_dir() && own && found.mode() & 0o077 == 0) {
            return Err(SpillError::Shared);
        }
    }
    let directory = super::canonical(&directory).map_err(SpillError::At)?;
    let path = directory.join(&name.file);
    let text = |part: &std::ffi::OsStr| part.to_str().map(str::to_owned);
    let components = path.components().map(|part| text(part.as_os_str()));
    let spelled = components
        .collect::<Option<_>>()
        .zip(text(path.as_os_str()));
    let (components, path) = spelled.ok_or(SpillError::NotUnicode)?;
    let path = TargetPath::new(path);
    Ok(SpillFile { path, components })
}

/// Create `name` under `root` for its user alone, failing if it exists: a file that
/// grows where it is read, which a staged file cannot be.
pub fn create(root: Option<&Path>, name: &SpillName) -> Result<(fs::File, TargetPath), SpillError> {
    let path = place(root, name)?.path;
    let mut options = fs::OpenOptions::new();
    options.append(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    match options.open(path.native()) {
        Ok(file) => Ok((file, path)),
        Err(error) => Err(SpillError::At(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    /// A name's parts are plain file names, whatever the wire carries, and a spill
    /// is private to its user and never replaces a file.
    #[test]
    fn names_cannot_traverse_and_spills_are_private_and_exclusive() {
        for (session, file) in [
            ("..", "a"),
            ("a", "."),
            ("a/b", "c"),
            ("a", "b\\c"),
            ("", "a"),
        ] {
            let wire = (session.to_owned(), file.to_owned());
            assert_eq!(SpillName::try_from(wire), Err(InvalidSpillName));
        }
        let root = tempfile::tempdir().unwrap();
        let name = SpillName::unique(&"session", "job1-body");
        assert_ne!(name, SpillName::unique(&"session", "job1-body"));
        let (_, path) = create(Some(root.path()), &name).unwrap();
        let path = path.native();
        assert!(path.starts_with(root.path().join("session")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!((mode(path.parent().unwrap()), mode(path)), (0o700, 0o600));
        }
        let again = create(Some(root.path()), &name).unwrap_err();
        assert_matches!(again, SpillError::At(error)
            if error.kind() == io::ErrorKind::AlreadyExists);
        // A directory that cannot be made fails.
        let blocked = root.path().join("file");
        fs::write(&blocked, b"").unwrap();
        let failed = place(Some(&blocked), &name).unwrap_err();
        assert_matches!(failed, SpillError::At(_));
        assert_matches!(place(None, &name), Err(SpillError::NoCacheDirectory));
        // A directory others can enter, or a link to one elsewhere, is refused.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let wide = root.path().join("wide");
            fs::create_dir(&wide).unwrap();
            fs::set_permissions(&wide, fs::Permissions::from_mode(0o755)).unwrap();
            let link = root.path().join("link");
            std::os::unix::fs::symlink(root.path().join("session"), &link).unwrap();
            for planted in [wide, link] {
                let session = planted.file_name().unwrap().to_str().unwrap();
                let name = SpillName::unique(&session, "job1-body");
                let refused = place(Some(root.path()), &name).unwrap_err();
                assert_matches!(refused, SpillError::Shared);
            }
            // A session names its files as text, which this directory's path is not.
            let unspelled: &std::ffi::OsStr = std::os::unix::ffi::OsStrExt::from_bytes(b"caf\xe9");
            let unspelled = root.path().join(unspelled);
            let refused = place(Some(&unspelled), &name).unwrap_err();
            assert_matches!(refused, SpillError::NotUnicode);
        }
    }
}

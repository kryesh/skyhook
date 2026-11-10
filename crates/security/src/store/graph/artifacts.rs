//! Test artifact admission, shared by recording a test and writing its report: each
//! reference names a distinct regular file below the run's artifact root, reached
//! without following a symbolic link.

use std::{
    collections::BTreeSet,
    io::{self, Seek as _},
    path::Path,
};

use schemars::JsonSchema;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use skyhook_base::string_newtype;
use skyhook_worker::{
    fs::{OpenFile, open_regular_blocking},
    op::FsError,
};

use crate::store::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("artifact references are portable relative paths below the run's artifacts directory")]
pub(crate) struct InvalidArtifactReference;

string_newtype! {
    /// A file relative to `<run-directory>/artifacts/`, never a workspace/absolute path; `.` and
    /// `..` components are rejected
    #[derive(PartialOrd, Ord)]
    pub(crate) struct ArtifactReference(InvalidArtifactReference) = |reference| {
        let valid_component = |part: &str| {
            !part.is_empty()
                && !matches!(part, "." | "..")
                && part.len() <= 240
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        };
        if reference.len() > 1024 || !reference.split('/').all(valid_component) {
            Err(InvalidArtifactReference)
        } else {
            Ok(())
        }
    }, schema = schemars::json_schema!({
        "type": "string",
        "pattern": "^[A-Za-z0-9._-]{1,240}(/[A-Za-z0-9._-]{1,240})*$",
        "maxLength": 1024
    });
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ArtifactFault {
    #[error("referenced twice")]
    Duplicate,
    #[error("symbolic links are not followed")]
    Symlink,
    #[error("not a regular file")]
    NotRegular,
    #[error("changed since its test recorded it")]
    Changed,
    #[error(transparent)]
    Live(skyhook_worker::fs::LiveDatabase),
    #[error("{0}")]
    Io(io::Error),
}

impl From<FsError> for ArtifactFault {
    fn from(error: FsError) -> Self {
        match error {
            FsError::NotRegular => Self::NotRegular,
            // Artifacts are admitted without a size bound.
            FsError::TooLarge { .. } => Self::Io(io::ErrorKind::FileTooLarge.into()),
            FsError::Live(held) => Self::Live(held),
            FsError::Os(error) => Self::Io((&error).into()),
        }
    }
}

/// Open `reference` below `root`, refusing it when resolving it would follow a symbolic link
/// at `root` or below it: its plain components then resolve to exactly themselves.
fn open_unlinked(
    root: &Path,
    reference: &ArtifactReference,
) -> std::result::Result<OpenFile, ArtifactFault> {
    let canonical = |path: &Path| std::fs::canonicalize(path).map_err(ArtifactFault::Io);
    let resolved = canonical(&root.join(reference.as_str()))?;
    let linked = std::fs::symlink_metadata(root)
        .map_err(ArtifactFault::Io)?
        .is_symlink();
    if linked || resolved != canonical(root)?.join(reference.as_str()) {
        return Err(ArtifactFault::Symlink);
    }
    Ok(open_regular_blocking(&resolved, u64::MAX)?)
}

/// An admitted artifact, open for reading.
pub(crate) struct OpenArtifact {
    pub(crate) artifact: TestArtifact,
    file: OpenFile,
}

impl OpenArtifact {
    /// The artifact's contents from their start, as they read now; read them in chunks.
    pub(crate) fn contents(&mut self) -> io::Result<&std::fs::File> {
        self.file.rewind()?;
        Ok(&self.file)
    }

    /// The SHA-256 of the artifact's contents as they read now.
    pub(crate) fn digest(&mut self) -> io::Result<[u8; 32]> {
        let mut reader = Digesting::new(self.contents()?, &|| false);
        io::copy(&mut reader, &mut io::sink())?;
        Ok(reader.finish())
    }
}

/// Reads `inner` while taking the SHA-256 of what passes through, each chunk only while the
/// work is wanted, so cancelled work stops between chunks however long the artifact is.
pub(crate) struct Digesting<'a, R> {
    inner: R,
    hasher: Sha256,
    cancelled: &'a dyn Fn() -> bool,
}

impl<'a, R: io::Read> Digesting<'a, R> {
    pub(crate) fn new(inner: R, cancelled: &'a dyn Fn() -> bool) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            cancelled,
        }
    }

    pub(crate) fn finish(self) -> [u8; 32] {
        self.hasher.finalize().into()
    }
}

/// Why a chunked read stopped early: the caller's cancellation.
#[derive(Debug, thiserror::Error)]
#[error("cancelled")]
pub(crate) struct Stopped;

impl<R: io::Read> io::Read for Digesting<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if (self.cancelled)() {
            return Err(io::Error::other(Stopped));
        }
        let read = self.inner.read(buffer)?;
        self.hasher.update(&buffer[..read]);
        Ok(read)
    }
}

/// Why artifacts are not admitted.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Inadmissible {
    #[error("exceeds {MAX_ARTIFACTS} artifacts per test")]
    TooMany,
    #[error("test artifact {reference}: {fault}")]
    Fault {
        reference: ArtifactReference,
        fault: ArtifactFault,
    },
}

/// Admit every artifact or refuse with the first fault.
pub(crate) fn admit(
    root: &Path,
    artifacts: &[TestArtifact],
) -> std::result::Result<Vec<OpenArtifact>, Inadmissible> {
    if artifacts.len() > MAX_ARTIFACTS {
        return Err(Inadmissible::TooMany);
    }
    let mut seen = BTreeSet::new();
    let mut admitted = Vec::new();
    for artifact in artifacts {
        let fault = |fault| Inadmissible::Fault {
            reference: artifact.reference.clone(),
            fault,
        };
        if !seen.insert(&artifact.reference) {
            return Err(fault(ArtifactFault::Duplicate));
        }
        let file = open_unlinked(root, &artifact.reference).map_err(fault)?;
        admitted.push(OpenArtifact {
            artifact: artifact.clone(),
            file,
        });
    }
    Ok(admitted)
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TestArtifact {
    /// Must be a regular file
    pub(crate) reference: ArtifactReference,
    pub(crate) role: ArtifactRole,
}

/// Maximum number of run-owned artifact references accepted by one test.
pub(crate) const MAX_ARTIFACTS: usize = 64;

#[cfg(test)]
mod tests {
    use std::{assert_matches, os::unix::fs::symlink};

    use super::*;

    /// A reference reaching its file through a symbolic link is refused, whether the link
    /// leads outside the root or stays below it, and one stepping out of the root never parses.
    #[test]
    fn references_reached_through_a_link_are_refused() {
        let (session, outside) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let root = session.path().join("artifacts");
        std::fs::create_dir_all(root.join("dir")).unwrap();
        std::fs::write(root.join("dir/kept.txt"), "kept").unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        symlink(outside.path(), root.join("out")).unwrap();
        symlink(root.join("dir"), root.join("alias")).unwrap();
        let admit = |reference: &str| {
            let artifact = TestArtifact {
                reference: reference.parse().unwrap(),
                role: ArtifactRole::Attack,
            };
            admit(&root, &[artifact]).map(drop)
        };
        admit("dir/kept.txt").unwrap();
        for linked in ["out/secret.txt", "alias/kept.txt"] {
            assert_matches!(
                admit(linked),
                Err(Inadmissible::Fault {
                    fault: ArtifactFault::Symlink,
                    ..
                }),
                "{linked}"
            );
        }
        assert!("dir/../../escape".parse::<ArtifactReference>().is_err());
        assert!("/etc/passwd".parse::<ArtifactReference>().is_err());
    }
}

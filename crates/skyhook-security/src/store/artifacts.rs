//! Test artifact admission, shared by recording a test and writing its report: each
//! reference names a distinct regular file below the run's artifact root, reached
//! without following a symbolic link.

use std::{
    collections::BTreeSet,
    io::{self, Seek as _},
    path::Path,
};

use sha2::{Digest as _, Sha256};
use skyhook::fs::{BeneathError, OpenFile, RegularFileError, open_beneath_blocking};

use super::*;

#[derive(Debug, thiserror::Error)]
pub enum ArtifactFault {
    #[error("referenced twice")]
    Duplicate,
    #[error("symbolic links are not followed")]
    Symlink,
    #[error("not a regular file")]
    NotRegular,
    #[error("changed since its test recorded it")]
    Changed,
    #[error(transparent)]
    Live(skyhook::fs::LiveDatabase),
    #[error("{0}")]
    Io(io::Error),
}

impl From<BeneathError> for ArtifactFault {
    fn from(error: BeneathError) -> Self {
        match error {
            BeneathError::Symlink => Self::Symlink,
            BeneathError::File(RegularFileError::NotRegular) => Self::NotRegular,
            // Artifacts are admitted without a size bound.
            BeneathError::File(RegularFileError::TooLarge { .. }) => {
                Self::Io(io::ErrorKind::FileTooLarge.into())
            }
            BeneathError::File(RegularFileError::Live(held)) => Self::Live(held),
            BeneathError::File(RegularFileError::Io(error)) => Self::Io(error),
        }
    }
}

/// An admitted artifact, open for reading.
pub struct OpenArtifact {
    pub artifact: TestArtifact,
    file: OpenFile,
}

impl OpenArtifact {
    /// The artifact's contents from their start, as they read now; read them in chunks.
    pub fn contents(&mut self) -> io::Result<&std::fs::File> {
        self.file.rewind()?;
        Ok(&self.file)
    }

    /// The SHA-256 of the artifact's contents as they read now.
    pub fn digest(&mut self) -> io::Result<[u8; 32]> {
        let mut reader = Digesting::new(self.contents()?, &|| false);
        io::copy(&mut reader, &mut io::sink())?;
        Ok(reader.finish())
    }
}

/// Reads `inner` while taking the SHA-256 of what passes through, each chunk only while the
/// work is wanted, so cancelled work stops between chunks however long the artifact is.
pub struct Digesting<'a, R> {
    inner: R,
    hasher: Sha256,
    cancelled: &'a dyn Fn() -> bool,
}

impl<'a, R: io::Read> Digesting<'a, R> {
    pub fn new(inner: R, cancelled: &'a dyn Fn() -> bool) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            cancelled,
        }
    }

    pub fn finish(self) -> [u8; 32] {
        self.hasher.finalize().into()
    }
}

/// Why a chunked read stopped early: the caller's cancellation.
#[derive(Debug, thiserror::Error)]
#[error("cancelled")]
pub struct Stopped;

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
pub enum Inadmissible {
    #[error("exceeds {MAX_ARTIFACTS} artifacts per test")]
    TooMany,
    #[error("test artifact {reference}: {fault}")]
    Fault {
        reference: ArtifactReference,
        fault: ArtifactFault,
    },
}

/// Admit every artifact or refuse with the first fault.
pub fn admit(
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
        let file = open_beneath_blocking(root, Path::new(artifact.reference.as_str()), u64::MAX)
            .map_err(|error| fault(error.into()))?;
        admitted.push(OpenArtifact {
            artifact: artifact.clone(),
            file,
        });
    }
    Ok(admitted)
}

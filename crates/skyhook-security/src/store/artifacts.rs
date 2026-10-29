//! PoC artifact admission, shared by recording an attempt and publishing it: each
//! reference names a distinct regular file below the run's artifact root, reached
//! without following a symbolic link.

use std::{
    collections::BTreeSet,
    io::{self, Seek as _},
    path::Path,
};

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
    #[error("changed while it was published")]
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
    pub artifact: PocArtifact,
    file: OpenFile,
}

impl OpenArtifact {
    /// The artifact's contents from their start, as they read now; read them in chunks.
    pub fn contents(&mut self) -> io::Result<&std::fs::File> {
        self.file.rewind()?;
        Ok(&self.file)
    }
}

/// Admit every artifact or refuse with the first fault.
pub fn admit_artifacts(root: &Path, artifacts: &[PocArtifact]) -> Result<Vec<OpenArtifact>> {
    if artifacts.len() > MAX_ARTIFACTS {
        return Err(Error::Limit(Bound::Artifacts));
    }
    let mut seen = BTreeSet::new();
    let mut admitted = Vec::new();
    for artifact in artifacts {
        let fault = |fault| Error::Artifact {
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

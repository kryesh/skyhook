use std::path::Path;

use crate::tool::diagnostic::{Effects, Operation, PartialContext, PathRole, Subject};
use crate::tool::invocation::{AdmissionError, LocalError, OperationError};

pub(super) fn relative_path(workspace: &Path, path: &Path) -> String {
    match path.strip_prefix(workspace) {
        Ok(path) => {
            let text = path.to_string_lossy();
            if text.is_empty() {
                crate::tool::registry::DEFAULT_PATH.to_owned()
            } else {
                text.into_owned()
            }
        }
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

/// Whether a call created directories to hold what it makes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Directories {
    #[default]
    Existed,
    Created,
}

impl Directories {
    /// Create the missing directories above `path`.
    pub(super) async fn above(path: &Path) -> Result<Self, LocalError> {
        let Some(parent) = path.parent() else {
            return Ok(Self::Existed);
        };
        Self::create(parent, Subject::ParentDirectory(parent.to_owned())).await
    }

    /// Create `directory`, named as `subject`, and the missing directories above it.
    pub(crate) async fn create<O>(
        directory: &Path,
        subject: Subject,
    ) -> Result<Self, OperationError<O>> {
        if tokio::fs::metadata(directory)
            .await
            .is_ok_and(|found| found.is_dir())
        {
            return Ok(Self::Existed);
        }
        let context = PartialContext::new(Operation::CreateDirectories, subject);
        tokio::fs::create_dir_all(directory)
            .await
            .map_err(OperationError::annotated(
                context.effects(Effects::PartialChange),
            ))?;
        Ok(Self::Created)
    }

    /// `error` as it leaves the directories created for a destination it did not
    /// replace.
    pub(crate) fn left_by<O>(self, error: OperationError<O>) -> OperationError<O> {
        let replaced = error.diagnostic().context.effects == Effects::DestinationReplaced;
        if self == Self::Created && !replaced {
            error.effects(Effects::PartialChange)
        } else {
            error
        }
    }
}

pub(super) async fn atomic_write(
    path: &Path,
    contents: crate::fs::Contents,
) -> Result<u64, AdmissionError> {
    crate::fs::atomic_write(path, contents)
        .await
        .map_err(|error| {
            AdmissionError::io(error.source).context(atomic_write_context(path, error.stage))
        })
}

/// The operation, subject and known effects of a failed atomic replacement of `path`.
pub(super) fn atomic_write_context(
    path: &Path,
    stage: crate::fs::AtomicWriteStage,
) -> PartialContext {
    use crate::fs::AtomicWriteStage as Stage;
    let target = || Subject::path(path);
    let staging = || Subject::StagingFile(path.to_owned());
    let parent = || Subject::ParentDirectory(path.parent().unwrap_or(path).to_owned());
    // Only a completed rename proves replacement; commit and wait prove nothing.
    let (operation, subject, effects) = match stage {
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
        Stage::Wait => (Operation::Wait, target(), None),
        Stage::OpenDirectory => (
            Operation::OpenDirectory,
            parent(),
            Some(Effects::DestinationReplaced),
        ),
        Stage::SyncDirectory => (
            Operation::SyncDirectory,
            parent(),
            Some(Effects::DestinationReplaced),
        ),
    };
    let mut context = PartialContext::new(operation, subject);
    if let Some(effects) = effects {
        context = context.effects(effects);
    }
    if effects == Some(Effects::DestinationReplaced) {
        context = context.path(PathRole::Resolved, path);
    }
    context
}

use std::path::Path;

use crate::tool::diagnostic::{Effects, Operation, PartialContext, PathRole, Subject};
use crate::tool::invocation::AdmissionError;

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

pub(super) async fn atomic_write(
    path: &Path,
    contents: crate::fs::Contents,
) -> Result<u64, AdmissionError> {
    crate::fs::atomic_write(path, contents)
        .await
        .map_err(|error| atomic_write_error(path, error))
}

fn atomic_write_error(path: &Path, error: crate::fs::AtomicWriteError) -> AdmissionError {
    AdmissionError::io(error.source).context(atomic_write_context(path, error.stage))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn atomic_stages_preserve_io_and_report_only_known_commit_effects() {
        use crate::fs::{AtomicWriteError, AtomicWriteStage as Stage};
        let destination = Path::new("/workspace/destination");
        for (stage, operation, effects, subject) in [
            (
                Stage::CreateStaging,
                Operation::Create,
                Effects::Unchanged,
                Subject::StagingFile(destination.to_owned()),
            ),
            (
                Stage::Commit,
                Operation::Rename,
                Effects::Unknown,
                Subject::path(destination),
            ),
            (
                Stage::SyncDirectory,
                Operation::SyncDirectory,
                Effects::DestinationReplaced,
                Subject::ParentDirectory(PathBuf::from("/workspace")),
            ),
            (
                Stage::Wait,
                Operation::Wait,
                Effects::Unknown,
                Subject::path(destination),
            ),
        ] {
            let error = atomic_write_error(
                destination,
                AtomicWriteError {
                    stage,
                    source: std::io::Error::from_raw_os_error(13),
                },
            );
            let diagnostic = error.diagnostic();
            assert_eq!(diagnostic.context.operation, operation);
            assert_eq!(diagnostic.context.subject, subject);
            assert_eq!(diagnostic.context.effects, effects);
            assert!(matches!(
                diagnostic.cause,
                crate::tool::diagnostic::Cause::Io { code: Some(13), .. }
            ));
            if stage == Stage::SyncDirectory {
                assert!(
                    diagnostic
                        .context
                        .paths
                        .iter()
                        .any(|fact| fact.role == PathRole::Resolved && fact.path == destination)
                );
            }
        }
    }
}

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::tool::diagnostic::{DiagnosticContext, Effects, Operation, PathRole, Subject};
use crate::tool::invocation::AdmissionError;
use crate::tool::policy::{ApprovalGrant, Capability, PermissionUse, ResourceId};
use crate::tool::registry::PathKind;

#[derive(Clone, Debug)]
pub(crate) struct ResolvedWorkspacePath {
    pub path: PathBuf,
    pub directory: bool,
}

impl ResolvedWorkspacePath {
    /// The permission for this path, proposing a grant over a directory's
    /// descendants or the exact file.
    pub(crate) fn permission(&self, capability: Capability, target: &str) -> PermissionUse {
        let resource = ResourceId::path(target, &self.path);
        let grant = if self.directory {
            ApprovalGrant::descendants(capability, resource.clone())
        } else {
            ApprovalGrant::exact(capability, resource.clone())
        };
        PermissionUse::new(capability, resource).with_grant(grant)
    }
}

pub(crate) async fn resolve_for_authorization(
    workspace: &Path,
    input: &str,
    kind: PathKind,
) -> Result<ResolvedWorkspacePath, AdmissionError> {
    let path = match kind {
        PathKind::Existing => resolve_existing(workspace, input).await?,
        PathKind::Writable => resolve_writable(workspace, input).await?,
        PathKind::WritableWithParents => resolve_writable_with_parents(workspace, input).await?,
        PathKind::Removable => resolve_removable(workspace, input).await?,
    };
    let directory = match kind {
        PathKind::Writable | PathKind::WritableWithParents
            if !fs::try_exists(&path)
                .await
                .map_err(AdmissionError::annotated(inspect_resolved(&path)))? =>
        {
            false
        }
        _ => fs::symlink_metadata(&path)
            .await
            .map_err(AdmissionError::annotated(inspect_resolved(&path)))?
            .is_dir(),
    };
    Ok(ResolvedWorkspacePath { path, directory })
}

pub(crate) async fn resolve_existing(
    workspace: &Path,
    relative: &str,
) -> Result<PathBuf, AdmissionError> {
    let joined = lexical_path(workspace, relative)?;
    fs::canonicalize(&joined)
        .await
        .map_err(AdmissionError::annotated(DiagnosticContext::new(
            Operation::Canonicalize,
            Subject::path(&joined),
        )))
}

pub(crate) async fn resolve_writable(
    workspace: &Path,
    relative: &str,
) -> Result<PathBuf, AdmissionError> {
    let joined = lexical_path(workspace, relative)?;
    if fs::try_exists(&joined)
        .await
        .map_err(AdmissionError::annotated(DiagnosticContext::new(
            Operation::Inspect,
            Subject::path(&joined),
        )))?
    {
        return resolve_existing(workspace, relative).await;
    }
    within_canonical_parent(&joined).await
}

/// Resolve the parent's symlinks while keeping the final entry's own name.
async fn within_canonical_parent(joined: &Path) -> Result<PathBuf, AdmissionError> {
    let missing = |part: &str| {
        AdmissionError::Failed(format!("path has no {part}"))
            .operation(Operation::Canonicalize, Subject::path(joined))
    };
    let parent = joined.parent().ok_or_else(|| missing("parent"))?;
    let parent = fs::canonicalize(parent)
        .await
        .map_err(AdmissionError::annotated(DiagnosticContext::new(
            Operation::Canonicalize,
            Subject::ParentDirectory(parent.to_owned()),
        )))?;
    let name = joined.file_name().ok_or_else(|| missing("filename"))?;
    Ok(parent.join(name))
}

/// Resolve existing symlinks and parent traversal without creating anything.
/// Authorization must finish before the write handler creates missing directories.
pub(crate) async fn resolve_writable_with_parents(
    workspace: &Path,
    relative: &str,
) -> Result<PathBuf, AdmissionError> {
    let joined = lexical_path(workspace, relative)?;
    let mut resolved = PathBuf::new();
    let mut components = joined.components().peekable();
    while let Some(component) = components.next() {
        resolved.push(component.as_os_str());
        let subject = if components.peek().is_some() {
            Subject::ParentDirectory(resolved.clone())
        } else {
            Subject::path(&resolved)
        };
        match fs::canonicalize(&resolved).await {
            Ok(path) => resolved = path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // A dangling symlink is not a missing directory. Do not leave an
                // unresolved link in a path that will be checked by the policy.
                match fs::symlink_metadata(&resolved).await {
                    Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => {}
                    Ok(_) => {
                        return Err(
                            AdmissionError::Io(error).operation(Operation::Canonicalize, subject)
                        );
                    }
                    Err(error) => {
                        return Err(
                            AdmissionError::Io(error).operation(Operation::Inspect, subject)
                        );
                    }
                }
                if component == std::path::Component::ParentDir {
                    resolved.pop();
                    resolved.pop();
                }
            }
            Err(error) => {
                return Err(AdmissionError::Io(error).operation(Operation::Canonicalize, subject));
            }
        }
    }
    Ok(resolved)
}

pub(crate) fn lexical_path(workspace: &Path, relative: &str) -> Result<PathBuf, AdmissionError> {
    let path = Path::new(relative);
    if path.as_os_str().is_empty() {
        return Err(AdmissionError::Failed("path cannot be empty".to_owned())
            .operation(Operation::Validate, Subject::argument(["path"])));
    }
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(workspace.join(path))
}

pub(crate) async fn resolve_removable(
    workspace: &Path,
    relative: &str,
) -> Result<PathBuf, AdmissionError> {
    let joined = lexical_path(workspace, relative)?;
    // A final .. has no filename; resolve it before removing the directory entry.
    let joined = if joined.file_name().is_none() {
        fs::canonicalize(&joined)
            .await
            .map_err(AdmissionError::annotated(DiagnosticContext::new(
                Operation::Canonicalize,
                Subject::path(&joined),
            )))?
    } else {
        joined
    };
    let resolved = within_canonical_parent(&joined).await?;
    if resolved
        == fs::canonicalize(workspace)
            .await
            .map_err(AdmissionError::annotated(DiagnosticContext::new(
                Operation::Canonicalize,
                Subject::working_directory(workspace),
            )))?
    {
        return Err(
            AdmissionError::Failed("cannot remove the workspace root".to_owned())
                .operation(Operation::Remove, Subject::path(&resolved))
                .effects(Effects::Unchanged),
        );
    }
    fs::symlink_metadata(&resolved)
        .await
        .map_err(AdmissionError::annotated(inspect_resolved(&resolved)))?;
    Ok(resolved)
}

fn inspect_resolved(path: &Path) -> DiagnosticContext {
    DiagnosticContext::new(Operation::Inspect, Subject::path(path)).path(PathRole::Resolved, path)
}

pub(crate) fn relative_path(workspace: &Path, path: &Path) -> String {
    match path.strip_prefix(workspace) {
        Ok(path) => {
            let text = path.to_string_lossy();
            if text.is_empty() {
                ".".to_owned()
            } else {
                text.into_owned()
            }
        }
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

pub(super) async fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), AdmissionError> {
    crate::fs::atomic_write(path, bytes)
        .await
        .map_err(|error| atomic_write_error(path, error))
}

fn atomic_write_error(path: &Path, error: crate::fs::AtomicWriteError) -> AdmissionError {
    use crate::fs::AtomicWriteStage as Stage;
    let target = || Subject::path(path);
    let staging = || Subject::StagingFile(path.to_owned());
    let parent = || Subject::ParentDirectory(path.parent().unwrap_or(path).to_owned());
    // Only a completed rename proves replacement; commit and wait prove nothing.
    let (operation, subject, effects) = match error.stage {
        Stage::Prepare => (Operation::Prepare, target(), Effects::Unchanged),
        Stage::InspectDestination => (Operation::Inspect, target(), Effects::Unchanged),
        Stage::CreateStaging => (Operation::Create, staging(), Effects::Unchanged),
        Stage::WriteStaging => (Operation::Write, staging(), Effects::Unchanged),
        Stage::SetPermissions => (Operation::SetPermissions, staging(), Effects::Unchanged),
        Stage::SyncStaging => (Operation::SyncFile, staging(), Effects::Unchanged),
        Stage::Commit => (Operation::Rename, target(), Effects::Unknown),
        Stage::Wait => (Operation::Wait, target(), Effects::Unknown),
        Stage::OpenDirectory => (
            Operation::OpenDirectory,
            parent(),
            Effects::DestinationReplaced,
        ),
        Stage::SyncDirectory => (
            Operation::SyncDirectory,
            parent(),
            Effects::DestinationReplaced,
        ),
    };
    let mut context = DiagnosticContext::new(operation, subject).effects(effects);
    if effects == Effects::DestinationReplaced {
        context = context.path(PathRole::Resolved, path);
    }
    AdmissionError::Io(error.source).context(context)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            assert!(
                matches!(error.unannotated(), AdmissionError::Io(error) if error.raw_os_error() == Some(13))
            );
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

    #[tokio::test]
    async fn writable_admission_names_the_attempted_parent_not_the_destination() {
        let root = tempfile::tempdir().unwrap();
        let error = resolve_writable(root.path(), "missing/file.txt")
            .await
            .unwrap_err();
        let diagnostic = error.diagnostic();
        assert_eq!(diagnostic.context.operation, Operation::Canonicalize);
        assert_eq!(
            diagnostic.context.subject,
            Subject::ParentDirectory(root.path().join("missing"))
        );
        assert!(
            matches!(error.unannotated(), AdmissionError::Io(error) if error.kind() == std::io::ErrorKind::NotFound)
        );
    }

    #[tokio::test]
    async fn traversal_is_consistent_and_workspace_root_stays_protected() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).await.unwrap();
        fs::create_dir(workspace.join("nested")).await.unwrap();
        fs::write(root.path().join("outside"), "data")
            .await
            .unwrap();
        let outside = fs::canonicalize(root.path().join("outside")).await.unwrap();
        assert_eq!(
            resolve_existing(&workspace, "../outside").await.unwrap(),
            outside
        );
        assert_eq!(
            resolve_existing(&workspace, outside.to_str().unwrap())
                .await
                .unwrap(),
            outside
        );
        assert_eq!(
            resolve_writable(&workspace, "../new").await.unwrap(),
            root.path().join("new")
        );
        assert_eq!(
            resolve_existing(&workspace, "nested/..").await.unwrap(),
            fs::canonicalize(&workspace).await.unwrap()
        );
        for path in [".", "nested/..", "../workspace"] {
            assert!(
                resolve_removable(&workspace, path)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("workspace root")
            );
        }
    }
}

//! Path admission: resolve a requested path where the call runs, before authorization.
use std::path::{Path, PathBuf};

use tokio::fs;

use crate::fs::RegularFileError;
use crate::tool::diagnostic::{Effects, Operation, PartialContext, PathRole, Subject};
use crate::tool::invocation::AdmissionError;
use crate::tool::policy::{Capability, PathText, PermissionUse, ResourceId};
use crate::tool::registry::PathKind;

#[derive(Clone, Debug)]
pub(crate) struct ResolvedWorkspacePath {
    pub path: PathText,
    pub directory: bool,
}

impl ResolvedWorkspacePath {
    /// The permission for this path, proposing a grant over a directory's
    /// descendants or the exact file.
    pub(crate) fn permission(
        &self,
        capability: Capability,
        target: &crate::target::TargetRef,
    ) -> PermissionUse {
        let resource = ResourceId::path(target, &self.path);
        if self.directory {
            PermissionUse::descendants(capability, resource)
        } else {
            PermissionUse::exact(capability, resource)
        }
    }

    /// The permission this path needs beyond the authorization `root`, which
    /// its caller has already authorized as a whole.
    pub(crate) fn permission_outside(
        &self,
        root: &Path,
        capability: Capability,
        target: &crate::target::TargetRef,
    ) -> Option<PermissionUse> {
        (!self.path.as_path().starts_with(root)).then(|| self.permission(capability, target))
    }
}

/// Attribute a failure to open or read a requested source file. Its IO failures
/// carry source-filesystem provenance.
pub(crate) fn source_file(path: &Path) -> impl FnOnce(RegularFileError) -> AdmissionError + use<> {
    let subject = Subject::path(path);
    move |error| {
        match error {
            RegularFileError::Io(error) => AdmissionError::source_filesystem_io(error),
            error => error.into(),
        }
        .operation(Operation::Read, subject)
    }
}

pub(crate) async fn resolve_for_authorization(
    workspace: &Path,
    input: &str,
    kind: PathKind,
) -> Result<ResolvedWorkspacePath, AdmissionError> {
    let path = match kind {
        PathKind::Existing | PathKind::WorkingDirectory => {
            resolve_existing(workspace, input).await?
        }
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
    let path = PathText::new(&path)
        .map_err(|error| error.or(PartialContext::default().path(PathRole::Resolved, &path)))?;
    Ok(ResolvedWorkspacePath { path, directory })
}

async fn resolve_existing(workspace: &Path, relative: &str) -> Result<PathBuf, AdmissionError> {
    let joined = lexical_path(workspace, relative)?;
    fs::canonicalize(&joined)
        .await
        .map_err(AdmissionError::annotated(PartialContext::new(
            Operation::Canonicalize,
            Subject::path(&joined),
        )))
}

async fn resolve_writable(workspace: &Path, relative: &str) -> Result<PathBuf, AdmissionError> {
    let joined = lexical_path(workspace, relative)?;
    if fs::try_exists(&joined)
        .await
        .map_err(AdmissionError::annotated(PartialContext::new(
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
        AdmissionError::failed(format!("path has no {part}"))
            .operation(Operation::Canonicalize, Subject::path(joined))
    };
    let parent = joined.parent().ok_or_else(|| missing("parent"))?;
    let parent = fs::canonicalize(parent)
        .await
        .map_err(AdmissionError::annotated(PartialContext::new(
            Operation::Canonicalize,
            Subject::ParentDirectory(parent.to_owned()),
        )))?;
    let name = joined.file_name().ok_or_else(|| missing("filename"))?;
    Ok(parent.join(name))
}

/// Resolve existing symlinks and parent traversal without creating anything.
/// Authorization must finish before the write handler creates missing directories.
async fn resolve_writable_with_parents(
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
                            AdmissionError::io(error).operation(Operation::Canonicalize, subject)
                        );
                    }
                    Err(error) => {
                        return Err(
                            AdmissionError::io(error).operation(Operation::Inspect, subject)
                        );
                    }
                }
                if component == std::path::Component::ParentDir {
                    resolved.pop();
                    resolved.pop();
                }
            }
            Err(error) => {
                return Err(AdmissionError::io(error).operation(Operation::Canonicalize, subject));
            }
        }
    }
    Ok(resolved)
}

pub(crate) fn lexical_path(workspace: &Path, relative: &str) -> Result<PathBuf, AdmissionError> {
    let path = Path::new(relative);
    if path.as_os_str().is_empty() {
        return Err(AdmissionError::failed("path cannot be empty")
            .operation(Operation::Validate, Subject::argument(["path"])));
    }
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(workspace.join(path))
}

async fn resolve_removable(workspace: &Path, relative: &str) -> Result<PathBuf, AdmissionError> {
    let joined = lexical_path(workspace, relative)?;
    // A final .. has no filename; resolve it before removing the directory entry.
    let joined = if joined.file_name().is_none() {
        fs::canonicalize(&joined)
            .await
            .map_err(AdmissionError::annotated(PartialContext::new(
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
            .map_err(AdmissionError::annotated(PartialContext::new(
                Operation::Canonicalize,
                Subject::working_directory(workspace),
            )))?
    {
        return Err(AdmissionError::failed("cannot remove the workspace root")
            .operation(Operation::Remove, Subject::path(&resolved))
            .effects(Effects::Unchanged));
    }
    fs::symlink_metadata(&resolved)
        .await
        .map_err(AdmissionError::annotated(inspect_resolved(&resolved)))?;
    Ok(resolved)
}

fn inspect_resolved(path: &Path) -> PartialContext {
    PartialContext::new(Operation::Inspect, Subject::path(path)).path(PathRole::Resolved, path)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(matches!(
            diagnostic.cause,
            crate::tool::diagnostic::Cause::Io {
                kind: crate::tool::diagnostic::IoKind::NotFound,
                ..
            }
        ));
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

use std::path::{Path, PathBuf};

use tokio::fs;

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
        PathKind::Writable | PathKind::WritableWithParents if !fs::try_exists(&path).await? => {
            false
        }
        _ => fs::symlink_metadata(&path).await?.is_dir(),
    };
    Ok(ResolvedWorkspacePath { path, directory })
}

pub(crate) async fn resolve_existing(
    workspace: &Path,
    relative: &str,
) -> Result<PathBuf, AdmissionError> {
    let joined = lexical_path(workspace, relative)?;
    Ok(fs::canonicalize(joined).await?)
}

pub(crate) async fn resolve_writable(
    workspace: &Path,
    relative: &str,
) -> Result<PathBuf, AdmissionError> {
    let joined = lexical_path(workspace, relative)?;
    if fs::try_exists(&joined).await? {
        return resolve_existing(workspace, relative).await;
    }
    let parent = joined
        .parent()
        .ok_or_else(|| AdmissionError::Failed("path has no parent".to_owned()))?;
    let parent = fs::canonicalize(parent).await?;
    let name = joined
        .file_name()
        .ok_or_else(|| AdmissionError::Failed("path has no filename".to_owned()))?;
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
    for component in joined.components() {
        resolved.push(component.as_os_str());
        match fs::canonicalize(&resolved).await {
            Ok(path) => resolved = path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // A dangling symlink is not a missing directory. Do not leave an
                // unresolved link in a path that will be checked by the policy.
                match fs::symlink_metadata(&resolved).await {
                    Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => {}
                    Ok(_) => return Err(error.into()),
                    Err(error) => return Err(error.into()),
                }
                if component == std::path::Component::ParentDir {
                    resolved.pop();
                    resolved.pop();
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(resolved)
}

pub(crate) fn lexical_path(workspace: &Path, relative: &str) -> Result<PathBuf, AdmissionError> {
    let path = Path::new(relative);
    if path.as_os_str().is_empty() {
        return Err(AdmissionError::Failed("path cannot be empty".to_owned()));
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
        fs::canonicalize(joined).await?
    } else {
        joined
    };
    let parent = joined
        .parent()
        .ok_or_else(|| AdmissionError::Failed("path has no parent".to_owned()))?;
    let parent = fs::canonicalize(parent).await?;
    let name = joined
        .file_name()
        .ok_or_else(|| AdmissionError::Failed("path has no filename".to_owned()))?;
    let resolved = parent.join(name);
    if resolved == fs::canonicalize(workspace).await? {
        return Err(AdmissionError::Failed(
            "cannot remove the workspace root".to_owned(),
        ));
    }
    fs::symlink_metadata(&resolved).await?;
    Ok(resolved)
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
    Ok(crate::fs::atomic_write(path, bytes).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

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

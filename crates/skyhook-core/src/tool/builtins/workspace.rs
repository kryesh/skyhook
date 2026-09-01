use std::path::{Component, Path, PathBuf};

use tokio::{fs, io::AsyncWriteExt as _};

use crate::tool::ToolError;
use crate::tool::registry::PathKind;

#[derive(Clone, Debug)]
pub(crate) struct ResolvedWorkspacePath {
    pub path: PathBuf,
    pub directory: bool,
}

pub(crate) async fn resolve_for_authorization(
    workspace: &Path,
    input: &str,
    kind: PathKind,
) -> Result<ResolvedWorkspacePath, ToolError> {
    let path = match kind {
        PathKind::Existing => resolve_existing(workspace, input).await?,
        PathKind::Writable => resolve_writable(workspace, input).await?,
        PathKind::Removable => resolve_removable(workspace, input).await?,
    };
    let directory = match kind {
        PathKind::Writable if !fs::try_exists(&path).await? => false,
        _ => fs::symlink_metadata(&path).await?.is_dir(),
    };
    Ok(ResolvedWorkspacePath { path, directory })
}

pub(crate) async fn resolve_existing(
    workspace: &Path,
    relative: &str,
) -> Result<PathBuf, ToolError> {
    let joined = lexical_path(workspace, relative)?;
    Ok(fs::canonicalize(joined).await?)
}

pub(crate) async fn resolve_directory(
    workspace: &Path,
    relative: &str,
) -> Result<PathBuf, ToolError> {
    let path = resolve_existing(workspace, relative).await?;
    if !fs::metadata(&path).await?.is_dir() {
        return Err(ToolError::Failed(format!(
            "working directory is not a directory: {relative}"
        )));
    }
    Ok(path)
}

pub(crate) async fn resolve_writable(
    workspace: &Path,
    relative: &str,
) -> Result<PathBuf, ToolError> {
    let joined = lexical_path(workspace, relative)?;
    if fs::try_exists(&joined).await? {
        return resolve_existing(workspace, relative).await;
    }
    let parent = joined
        .parent()
        .ok_or_else(|| ToolError::Failed("path has no parent".to_owned()))?;
    let parent = fs::canonicalize(parent).await?;
    let name = joined
        .file_name()
        .ok_or_else(|| ToolError::Failed("path has no filename".to_owned()))?;
    Ok(parent.join(name))
}

fn lexical_path(workspace: &Path, relative: &str) -> Result<PathBuf, ToolError> {
    let path = Path::new(relative);
    if path.as_os_str().is_empty() {
        return Err(ToolError::Failed("path cannot be empty".to_owned()));
    }
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(ToolError::Failed(
            "relative path cannot contain `..`".to_owned(),
        ));
    }
    Ok(workspace.join(path))
}

pub(crate) async fn resolve_removable(
    workspace: &Path,
    relative: &str,
) -> Result<PathBuf, ToolError> {
    let joined = lexical_path(workspace, relative)?;
    let parent = joined
        .parent()
        .ok_or_else(|| ToolError::Failed("path has no parent".to_owned()))?;
    let parent = fs::canonicalize(parent).await?;
    let name = joined
        .file_name()
        .ok_or_else(|| ToolError::Failed("path has no filename".to_owned()))?;
    let resolved = parent.join(name);
    if resolved == workspace {
        return Err(ToolError::Failed(
            "cannot remove the workspace root".to_owned(),
        ));
    }
    fs::symlink_metadata(&resolved).await?;
    Ok(resolved)
}

pub(crate) fn relative_path(workspace: &Path, path: &Path) -> Result<String, ToolError> {
    Ok(match path.strip_prefix(workspace) {
        Ok(path) => {
            let text = path.to_string_lossy();
            if text.is_empty() {
                ".".to_owned()
            } else {
                text.into_owned()
            }
        }
        Err(_) => path.to_string_lossy().into_owned(),
    })
}

pub(super) async fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ToolError> {
    let parent = path
        .parent()
        .ok_or_else(|| ToolError::Failed("path has no parent".to_owned()))?;
    let mut random = [0; 8];
    getrandom::fill(&mut random).map_err(|error| ToolError::Failed(error.to_string()))?;
    let temporary = parent.join(format!(".skyhook-{:016x}.tmp", u64::from_ne_bytes(random)));
    let permissions = match fs::metadata(path).await {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .await?;
    if let Err(error) = async {
        file.write_all(bytes).await?;
        file.flush().await?;
        file.sync_all().await?;
        if let Some(permissions) = permissions {
            fs::set_permissions(&temporary, permissions).await?;
        }
        Ok::<(), std::io::Error>(())
    }
    .await
    {
        let _ = fs::remove_file(&temporary).await;
        return Err(error.into());
    }
    drop(file);
    if let Err(error) = fs::rename(&temporary, path).await {
        let _ = fs::remove_file(&temporary).await;
        return Err(error.into());
    }
    let parent = parent.to_owned();
    tokio::task::spawn_blocking(move || std::fs::File::open(parent)?.sync_all())
        .await
        .map_err(|error| ToolError::Failed(error.to_string()))??;
    Ok(())
}

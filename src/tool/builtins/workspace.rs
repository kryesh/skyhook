use std::path::{Component, Path, PathBuf};

use tokio::{fs, io::AsyncWriteExt as _};

use crate::tool::ToolError;

pub(super) async fn resolve_existing(
    workspace: &Path,
    relative: &str,
) -> Result<PathBuf, ToolError> {
    let joined = lexical_path(workspace, relative)?;
    let resolved = fs::canonicalize(joined).await?;
    ensure_contained(workspace, resolved)
}

pub(super) async fn resolve_directory(
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

pub(super) async fn resolve_writable(
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
    ensure_contained(workspace, parent)?;
    Ok(joined)
}

fn lexical_path(workspace: &Path, relative: &str) -> Result<PathBuf, ToolError> {
    let path = Path::new(relative);
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(ToolError::Failed(
            "path must be workspace-relative and cannot contain `..`".to_owned(),
        ));
    }
    Ok(workspace.join(path))
}

pub(super) async fn resolve_removable(
    workspace: &Path,
    relative: &str,
) -> Result<PathBuf, ToolError> {
    let joined = lexical_path(workspace, relative)?;
    if joined == workspace {
        return Err(ToolError::Failed(
            "cannot remove the workspace root".to_owned(),
        ));
    }
    let parent = joined
        .parent()
        .ok_or_else(|| ToolError::Failed("path has no parent".to_owned()))?;
    let parent = fs::canonicalize(parent).await?;
    ensure_contained(workspace, parent)?;
    fs::symlink_metadata(&joined).await?;
    Ok(joined)
}

pub(super) fn relative_path(workspace: &Path, path: &Path) -> Result<String, ToolError> {
    path.strip_prefix(workspace)
        .map(|path| {
            let text = path.to_string_lossy();
            if text.is_empty() {
                ".".to_owned()
            } else {
                text.into_owned()
            }
        })
        .map_err(|_| ToolError::Failed("path escapes the workspace".to_owned()))
}

fn ensure_contained(workspace: &Path, path: PathBuf) -> Result<PathBuf, ToolError> {
    if path.starts_with(workspace) {
        Ok(path)
    } else {
        Err(ToolError::Failed("path escapes the workspace".to_owned()))
    }
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

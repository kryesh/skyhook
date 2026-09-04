//! Shared atomic file replacement; callers select their durability guarantees.

use std::{io, path::Path};
use tokio::{fs, io::AsyncWriteExt as _};

#[derive(Default)]
pub(crate) struct AtomicWriteOptions {
    pub preserve_permissions: bool,
    pub sync_parent: bool,
}

pub(crate) async fn atomic_write(
    path: &Path,
    bytes: &[u8],
    options: AtomicWriteOptions,
) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent"))?;
    let permissions = if options.preserve_permissions {
        match fs::metadata(path).await {
            Ok(metadata) => Some(metadata.permissions()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    let mut random = [0; 8];
    getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
    let temporary = parent.join(format!(".skyhook-{:016x}.tmp", u64::from_ne_bytes(random)));
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .await?;
    let result = async {
        file.write_all(bytes).await?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions).await?;
        }
        file.sync_all().await?;
        drop(file);
        fs::rename(&temporary, path).await
    }
    .await;
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary).await;
        return Err(error);
    }
    if options.sync_parent {
        let parent = parent.to_owned();
        tokio::task::spawn_blocking(move || std::fs::File::open(parent)?.sync_all())
            .await
            .map_err(io::Error::other)??;
    }
    Ok(())
}

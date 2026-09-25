//! Private atomic credential persistence and cross-process refresh/login coordination.
use super::{AuthError, AuthManager, AuthStatus, MAX_BODY, blocking, error, random_string};
use crate::fs::{AtomicWriteStage, CommitMode, PermissionPolicy, StagedFile, sync_directory};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    time::Duration,
};
use zeroize::{Zeroize, Zeroizing};

#[derive(Serialize, Deserialize)]
pub(super) struct Stored {
    pub(super) version: u32,
    pub(super) access_token: String,
    pub(super) refresh_token: String,
    pub(super) account_id: String,
    pub(super) expires_at: u64,
}
impl Drop for Stored {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
    }
}

impl AuthManager {
    pub async fn status(&self) -> Result<AuthStatus, AuthError> {
        let (_lock, stored) = self.load().await?;
        Ok(match stored {
            Some(stored) => AuthStatus::LoggedIn {
                account_id: stored.account_id.clone(),
                expires_at: stored.expires_at,
            },
            None => AuthStatus::LoggedOut,
        })
    }

    pub async fn logout(&self) -> Result<(), AuthError> {
        let directory = self.inner.directory.clone();
        blocking(move || {
            let mut lock = lock_store(&directory)?;
            // Persist an epoch even when already logged out: an in-progress
            // login must not resurrect credentials after explicit logout.
            let epoch = random_string()?;
            lock.seek(SeekFrom::Start(0))
                .and_then(|_| lock.write_all(epoch.as_bytes()))
                .and_then(|_| lock.set_len(epoch.len() as u64))
                .and_then(|_| lock.sync_all())
                .map_err(|_| error("Cannot invalidate in-progress Codex login"))?;
            match fs::remove_file(directory.join("codex-oauth.json")) {
                Ok(()) => sync_directory(&directory).map_err(|_| directory_sync_error()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(_) => Err(error("Cannot remove Skyhook Codex credentials")),
            }
            // Do not remove the lock file: other processes may have its inode open.
        })
        .await
    }

    pub(super) async fn load(&self) -> Result<(File, Option<Stored>), AuthError> {
        let directory = self.inner.directory.clone();
        blocking(move || {
            let lock = lock_store(&directory)?;
            let stored = read_store(&directory)?;
            Ok((lock, stored))
        })
        .await
    }

    pub(super) async fn save(&self, lock: File, stored: Stored) -> Result<(), AuthError> {
        let directory = self.inner.directory.clone();
        blocking(move || {
            let _lock = lock;
            write_store(&directory, &stored)
        })
        .await
    }
}

// OS locking is performed in spawn_blocking, never on a Tokio worker. Bounded
// try-lock polling also bounds the lifetime of a cancelled blocking operation.
pub(super) fn lock_store(directory: &Path) -> Result<File, AuthError> {
    secure_directory(directory)?;
    let file = private_open(&directory.join("codex-oauth.lock"), true)
        .map_err(|_| error("Cannot open Skyhook Codex credential lock"))?;
    let start = std::time::Instant::now();
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    && start.elapsed() < Duration::from_secs(60) =>
            {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => {
                return Err(error(
                    "Cannot acquire Skyhook Codex credential lock; try again",
                ));
            }
        }
    }
}

#[cfg(unix)]
fn secure_directory(directory: &Path) -> Result<(), AuthError> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder
        .create(directory)
        .map_err(|_| error("Cannot create Skyhook credential directory"))?;
    let metadata = fs::symlink_metadata(directory)
        .map_err(|_| error("Cannot inspect Skyhook credential directory"))?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(error(
            "Skyhook credential directory must be an owned, non-symlink directory",
        ));
    }
    // Existing config directories may intentionally contain readable config.
    // Do not chmod unrelated configuration; private files protect token contents.
    if metadata.permissions().mode() & 0o022 != 0 {
        return Err(error(
            "Skyhook credential directory must not be writable by other users",
        ));
    }
    Ok(())
}
#[cfg(not(unix))]
fn secure_directory(_directory: &Path) -> Result<(), AuthError> {
    // Do not pretend Unix mode bits provide a private ACL on another OS.
    Err(error(
        "Private Skyhook Codex credential storage is currently supported on Unix only",
    ))
}

fn private_open(path: &Path, create: bool) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(create).create(create);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let metadata = crate::fs::admit_regular(&file, u64::MAX)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::PermissionDenied))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err(std::io::ErrorKind::PermissionDenied.into());
        }
    }
    Ok(file)
}

fn read_store(directory: &Path) -> Result<Option<Stored>, AuthError> {
    let file = match private_open(&directory.join("codex-oauth.json"), false) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(error(
                "Cannot read Skyhook Codex credentials; require an owned private regular file",
            ));
        }
    };
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_BODY as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| error("Cannot read Skyhook Codex credentials"))?;
    if bytes.len() > MAX_BODY {
        return Err(error("Skyhook Codex credential file is too large"));
    }
    let stored: Stored = serde_json::from_slice(&bytes)
        .map_err(|_| error("Skyhook Codex credentials are malformed; log in again"))?;
    if stored.version != 1
        || !valid_token(&stored.access_token)
        || !valid_token(&stored.refresh_token)
        || !valid_account(&stored.account_id)
    {
        return Err(error("Skyhook Codex credentials are invalid; log in again"));
    }
    Ok(Some(stored))
}

pub(super) fn write_store(directory: &Path, stored: &Stored) -> Result<(), AuthError> {
    let bytes = Zeroizing::new(
        serde_json::to_vec(stored)
            .map_err(|_| error("Cannot serialize Skyhook Codex credentials"))?,
    );
    let mut staged = StagedFile::create(
        &directory.join("codex-oauth.json"),
        PermissionPolicy::Private,
    )
    .map_err(|_| error("Cannot create private Skyhook credential file"))?;
    staged
        .write(&bytes)
        .map_err(|_| error("Cannot write Skyhook Codex credentials"))?;
    staged
        .commit(CommitMode::Replace)
        .map_err(|failure| match failure.stage {
            AtomicWriteStage::OpenDirectory | AtomicWriteStage::SyncDirectory => {
                directory_sync_error()
            }
            _ => error("Cannot atomically save Skyhook Codex credentials"),
        })
}
fn directory_sync_error() -> AuthError {
    error("Cannot sync Skyhook credential directory")
}
pub(super) fn read_epoch(mut lock: &File) -> Result<Vec<u8>, AuthError> {
    lock.seek(SeekFrom::Start(0))
        .map_err(|_| error("Cannot inspect Codex login generation"))?;
    let mut epoch = Vec::new();
    lock.take(128)
        .read_to_end(&mut epoch)
        .map_err(|_| error("Cannot read Codex login generation"))?;
    Ok(epoch)
}

pub(super) fn store_fingerprint(stored: &Stored) -> Vec<u8> {
    let mut hash = Sha256::new();
    hash.update(stored.access_token.as_bytes());
    hash.update([0]);
    hash.update(stored.refresh_token.as_bytes());
    hash.finalize().to_vec()
}

pub(super) fn valid_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 128 * 1024
        && !token.chars().any(char::is_whitespace)
        && !token.chars().any(char::is_control)
}
pub(super) fn valid_account(account: &str) -> bool {
    !account.is_empty()
        && account.len() <= 256
        && account
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(all(test, unix))]
pub(super) mod tests {
    use super::super::now;
    use super::*;

    pub(in super::super) fn stored(expiry: u64) -> Stored {
        Stored {
            version: 1,
            access_token: "old-access".into(),
            refresh_token: "old-refresh".into(),
            account_id: "account-123".into(),
            expires_at: expiry,
        }
    }

    #[tokio::test]
    async fn persisted_private_atomic_credentials_and_logout() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let manager =
            AuthManager::at(temp.path().join("skyhook"), "http://127.0.0.1:1".into()).unwrap();
        assert_eq!(manager.status().await.unwrap(), AuthStatus::LoggedOut);
        assert!(
            manager
                .credentials()
                .await
                .unwrap_err()
                .message
                .contains("skyhook auth login codex")
        );
        let (lock, _) = manager.load().await.unwrap();
        manager
            .save(lock, stored(now().unwrap() + 3600))
            .await
            .unwrap();
        let path = manager.inner.directory.join("codex-oauth.json");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&manager.inner.directory)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            manager.credentials().await.unwrap().access_token,
            "old-access"
        );
        assert!(
            matches!(manager.status().await.unwrap(), AuthStatus::LoggedIn{account_id, ..} if account_id == "account-123")
        );
        assert!(!format!("{:?}", manager.credentials().await.unwrap()).contains("old-access"));
        manager.logout().await.unwrap();
        let (lock, _) = manager.load().await.unwrap();
        let epoch = read_epoch(&lock).unwrap();
        drop(lock);
        // Even an already-empty store must invalidate an in-progress login.
        manager.logout().await.unwrap();
        let (lock, stored) = manager.load().await.unwrap();
        assert!(stored.is_none());
        assert_ne!(epoch, read_epoch(&lock).unwrap());
        drop(lock);
        assert!(!path.exists());
        assert!(manager.inner.directory.join("codex-oauth.lock").exists());
        assert_eq!(manager.status().await.unwrap(), AuthStatus::LoggedOut);
    }

    #[test]
    fn private_store_rejects_symlinks_permissive_files_and_invalid_json() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("codex-oauth.json");
        let unrelated = temp.path().join("unrelated");
        fs::write(&unrelated, b"DO-NOT-READ").unwrap();
        symlink(&unrelated, &path).unwrap();
        assert!(read_store(temp.path()).is_err());
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"TOP-SECRET invalid JSON").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_store(temp.path()).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let err = read_store(temp.path()).err().unwrap();
        assert!(!format!("{err:?}").contains("TOP-SECRET"));
    }

    #[test]
    fn existing_configuration_permissions_are_not_changed() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755)).unwrap();
        secure_directory(temp.path()).unwrap();
        assert_eq!(
            fs::metadata(temp.path()).unwrap().permissions().mode() & 0o777,
            0o755
        );
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o777)).unwrap();
        assert!(secure_directory(temp.path()).is_err());
    }
}

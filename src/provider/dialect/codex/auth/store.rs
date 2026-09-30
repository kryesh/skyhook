//! Private atomic credential persistence and cross-process refresh/login coordination.
use super::{AuthManager, Issuer, blocking};
use crate::fs::{AtomicWriteStage, CommitMode, PermissionPolicy, StagedFile};
use crate::newtype::string_newtype;
use crate::provider::{
    ProviderError,
    ProviderErrorKind::{Authentication, Unavailable},
    dialect::{AuthStatus, LoginReason, LoginRequired},
    profile::ProviderName,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};
use zeroize::Zeroizing;

const FORMAT: u32 = 1;
const MAX_STORE_BYTES: u64 = 1024 * 1024;
/// The longest provider name that is a file name beside `.json` or `.lock`.
const MAX_NAME_BYTES: usize = 250;
const MAX_TOKEN_BYTES: usize = 128 * 1024;
const MAX_ACCOUNT_BYTES: usize = 256;
const LOCK_TIMEOUT: Duration = Duration::from_secs(60);
const LOCK_POLL: Duration = Duration::from_millis(25);

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid Codex token")]
pub struct InvalidToken;

/// An OAuth token: 1 byte to 128 KiB, without whitespace or control
/// characters. Absent from Debug and wiped on drop.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct Token(Zeroizing<String>);

impl Token {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Token {
    type Error = InvalidToken;

    fn try_from(token: String) -> Result<Self, InvalidToken> {
        let token = Zeroizing::new(token);
        let valid = !token.is_empty()
            && token.len() <= MAX_TOKEN_BYTES
            && !token.chars().any(|c| c.is_whitespace() || c.is_control());
        valid.then(|| Self(token)).ok_or(InvalidToken)
    }
}

impl Serialize for Token {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid ChatGPT account ID")]
pub struct InvalidAccount;

string_newtype! {
    /// A ChatGPT account ID: 1 to 256 ASCII letters, digits, hyphens or underscores.
    pub struct AccountId(InvalidAccount) = |account| {
        let valid = !account.is_empty()
            && account.len() <= MAX_ACCOUNT_BYTES
            && account.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        valid.then_some(()).ok_or(InvalidAccount)
    };
}

/// The record of the current format; `write_store` adds the version.
#[derive(Serialize, Deserialize)]
pub(super) struct Stored {
    /// The issuer that granted these tokens; they are used with no other.
    pub(super) issuer: Issuer,
    pub(super) access_token: Token,
    pub(super) refresh_token: Token,
    pub(super) account_id: AccountId,
    pub(super) expires_at: u64,
}

/// One provider's credential file and the lock every process holds while it
/// reads, refreshes or writes them, named after the provider in a private
/// directory.
#[derive(Clone)]
pub(super) struct Files {
    directory: PathBuf,
    store: PathBuf,
    lock: PathBuf,
}

/// A provider name no credential file can carry.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "provider `{0}` cannot name a credentials file; use a name without NUL of at most {MAX_NAME_BYTES} bytes"
)]
pub struct UnfileableName(ProviderName);

impl From<UnfileableName> for ProviderError {
    fn from(error: UnfileableName) -> Self {
        Authentication.error(error.to_string())
    }
}

impl Files {
    /// Provider names hold no `/`, so each is one file name once it fits the
    /// file system's limit beside its extension.
    pub(super) fn new(directory: PathBuf, provider: &ProviderName) -> Result<Self, UnfileableName> {
        let name = provider.as_str();
        if name.len() > MAX_NAME_BYTES || name.contains('\0') {
            return Err(UnfileableName(provider.clone()));
        }
        Ok(Self {
            store: directory.join(format!("{provider}.json")),
            lock: directory.join(format!("{provider}.lock")),
            directory,
        })
    }
}

impl AuthManager {
    pub(in crate::provider::dialect) async fn status(&self) -> Result<AuthStatus, ProviderError> {
        Ok(match self.usable(self.read().await?) {
            Ok((_, stored)) => AuthStatus::LoggedIn {
                expires_at: stored.expires_at,
            },
            Err(required) => AuthStatus::LoginRequired(required),
        })
    }

    /// The saved credentials under their lock, unless absent or another issuer granted them.
    pub(super) fn usable(
        &self,
        saved: Option<(File, Stored)>,
    ) -> Result<(File, Stored), LoginRequired> {
        match saved {
            Some((lock, stored)) if stored.issuer == self.inner.issuer => Ok((lock, stored)),
            Some(_) => Err(self.required(LoginReason::OtherIssuer)),
            None => Err(self.required(LoginReason::LoggedOut)),
        }
    }

    pub(in crate::provider::dialect) fn required(&self, reason: LoginReason) -> LoginRequired {
        LoginRequired {
            command: self.inner.command.clone(),
            reason,
        }
    }

    /// The saved credentials under the held lock; reading creates nothing.
    pub(super) async fn read(&self) -> Result<Option<(File, Stored)>, ProviderError> {
        let files = self.inner.files.clone();
        blocking(move || {
            let Some(lock) = lock_saved(&files)? else {
                return Ok(None);
            };
            Ok(read_store(&files)?.map(|stored| (lock, stored)))
        })
        .await
    }

    /// The held lock and the saved credentials, if any, creating the store.
    pub(super) async fn load(&self) -> Result<(File, Option<Stored>), ProviderError> {
        let files = self.inner.files.clone();
        blocking(move || {
            let lock = lock_store(&files)?;
            let stored = read_store(&files)?;
            Ok((lock, stored))
        })
        .await
    }

    pub(super) async fn save(&self, lock: File, stored: Stored) -> Result<(), ProviderError> {
        let files = self.inner.files.clone();
        blocking(move || {
            let _lock = lock;
            write_store(&files, &stored)
        })
        .await
    }
}

/// Lock the store, creating its private directory and lock file.
pub(super) fn lock_store(files: &Files) -> Result<File, ProviderError> {
    create_directory(&files.directory)?;
    lock(files)
}

/// Lock the store where credentials are saved; nothing is created where none
/// are. A saved file whose lock is missing, as a restore may leave it, gets one.
fn lock_saved(files: &Files) -> Result<Option<File>, ProviderError> {
    if !private_directory(&files.directory)? {
        return Ok(None);
    }
    match fs::symlink_metadata(&files.store) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(Authentication.error("Cannot inspect Skyhook Codex credentials")),
    }
    lock(files).map(Some)
}

// OS locking is performed in spawn_blocking, never on a Tokio worker. Bounded
// try-lock polling also bounds the lifetime of a cancelled blocking operation.
fn lock(files: &Files) -> Result<File, ProviderError> {
    let lock = private_open(&files.lock, true)
        .map_err(|_| Authentication.error("Cannot open Skyhook Codex credential lock"))?;
    let start = std::time::Instant::now();
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => return Ok(lock),
            Err(e) if e.kind() != std::io::ErrorKind::WouldBlock => {
                return Err(Authentication.error("Cannot lock Skyhook Codex credentials"));
            }
            Err(_) if start.elapsed() >= LOCK_TIMEOUT => {
                let unavailable = Unavailable;
                return Err(
                    unavailable.error("Cannot acquire Skyhook Codex credential lock; try again")
                );
            }
            Err(_) => std::thread::sleep(LOCK_POLL),
        }
    }
}

#[cfg(unix)]
fn create_directory(directory: &Path) -> Result<(), ProviderError> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .map_err(|_| Authentication.error("Cannot create Skyhook credential directory"))?;
    private_directory(directory).map(drop)
}

/// Whether the credential directory exists; it must be private to its owner.
#[cfg(unix)]
fn private_directory(directory: &Path) -> Result<bool, ProviderError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(Authentication.error("Cannot inspect Skyhook credential directory")),
    };
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(Authentication
            .error("Skyhook credential directory must be an owned, non-symlink directory"));
    }
    // Skyhook makes it 0700; one made elsewhere is refused rather than chmodded.
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(Authentication.error(format!(
            "Skyhook credential directory {} must be private to its owner (chmod 700)",
            directory.display()
        )));
    }
    Ok(true)
}

// Do not pretend Unix mode bits provide a private ACL on another OS.
#[cfg(not(unix))]
fn create_directory(_directory: &Path) -> Result<(), ProviderError> {
    Err(unsupported())
}
#[cfg(not(unix))]
fn private_directory(_directory: &Path) -> Result<bool, ProviderError> {
    Err(unsupported())
}
#[cfg(not(unix))]
fn unsupported() -> ProviderError {
    Authentication
        .error("Private Skyhook Codex credential storage is currently supported on Unix only")
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

fn read_store(files: &Files) -> Result<Option<Stored>, ProviderError> {
    let file = match private_open(&files.store, false) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(Authentication.error(
                "Cannot read Skyhook Codex credentials; require an owned private regular file",
            ));
        }
    };
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_STORE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Authentication.error("Cannot read Skyhook Codex credentials"))?;
    if bytes.len() as u64 > MAX_STORE_BYTES {
        return Err(Authentication.error("Skyhook Codex credential file is too large"));
    }
    let malformed =
        || Authentication.error("Skyhook Codex credentials are malformed; log in again");
    #[derive(Deserialize)]
    struct Version {
        version: u32,
    }
    let Version { version } = serde_json::from_slice(&bytes).map_err(|_| malformed())?;
    if version != FORMAT {
        return Err(malformed());
    }
    let stored = serde_json::from_slice(&bytes).map_err(|_| malformed())?;
    Ok(Some(stored))
}

pub(super) fn write_store(files: &Files, stored: &Stored) -> Result<(), ProviderError> {
    #[derive(Serialize)]
    struct Versioned<'a> {
        version: u32,
        #[serde(flatten)]
        stored: &'a Stored,
    }
    let versioned = Versioned {
        version: FORMAT,
        stored,
    };
    let bytes = Zeroizing::new(
        serde_json::to_vec(&versioned)
            .map_err(|_| Authentication.error("Cannot serialize Skyhook Codex credentials"))?,
    );
    let mut staged = StagedFile::create(&files.store, PermissionPolicy::Private)
        .map_err(|_| Authentication.error("Cannot create private Skyhook credential file"))?;
    staged
        .write(&bytes)
        .map_err(|_| Authentication.error("Cannot write Skyhook Codex credentials"))?;
    staged
        .commit(CommitMode::Replace)
        .map_err(|failure| match failure.stage {
            AtomicWriteStage::OpenDirectory | AtomicWriteStage::SyncDirectory => {
                Authentication.error("Cannot sync Skyhook credential directory")
            }
            _ => Authentication.error("Cannot atomically save Skyhook Codex credentials"),
        })
}
pub(super) fn store_fingerprint(stored: &Stored) -> Vec<u8> {
    let mut hash = Sha256::new();
    hash.update(stored.access_token.as_str());
    hash.update([0]);
    hash.update(stored.refresh_token.as_str());
    hash.finalize().to_vec()
}

#[cfg(all(test, unix))]
pub(super) mod tests {
    use super::super::{login_command, manager_at, now};
    use super::*;

    pub(in super::super) fn stored(issuer: &Issuer, expiry: u64) -> Stored {
        Stored {
            issuer: issuer.clone(),
            access_token: Token::try_from("old-access".to_owned()).unwrap(),
            refresh_token: Token::try_from("old-refresh".to_owned()).unwrap(),
            account_id: "account-123".parse().unwrap(),
            expires_at: expiry,
        }
    }

    /// Each provider keeps its own credentials in a shared directory; reading
    /// creates nothing, and deleting a provider's file signs it out.
    #[tokio::test]
    async fn persisted_private_atomic_credentials_per_provider() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("credentials");
        let issuer = Issuer::parse("http://127.0.0.1:1").unwrap();
        let manager = manager_at(temp.path(), &issuer);
        let command = login_command(temp.path(), "other");
        let other = AuthManager::new(command, issuer.clone()).unwrap();
        let logged_out = |manager: &AuthManager| manager.required(LoginReason::LoggedOut);
        let status = |manager| AuthStatus::LoginRequired(logged_out(manager));
        assert_eq!(manager.status().await.unwrap(), status(&manager));
        let error = manager.credentials().await.unwrap_err();
        assert_eq!(error, logged_out(&manager).into());
        assert!(!directory.exists());
        let (lock, _) = manager.load().await.unwrap();
        manager
            .save(lock, stored(&issuer, now().unwrap() + 3600))
            .await
            .unwrap();
        let path = directory.join("test.json");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            manager.credentials().await.unwrap().access_token.as_str(),
            "old-access"
        );
        assert!(matches!(
            manager.status().await.unwrap(),
            AuthStatus::LoggedIn { .. }
        ));
        assert!(!format!("{:?}", manager.credentials().await.unwrap()).contains("old-access"));
        assert_eq!(other.status().await.unwrap(), status(&other));
        assert!(!directory.join("other.lock").exists());
        // A restore that skips the lock file still reads as signed in.
        fs::remove_file(directory.join("test.lock")).unwrap();
        assert!(matches!(
            manager.status().await.unwrap(),
            AuthStatus::LoggedIn { .. }
        ));
        fs::remove_file(&path).unwrap();
        assert_eq!(manager.status().await.unwrap(), status(&manager));
    }

    #[test]
    fn private_store_rejects_symlinks_permissive_files_and_invalid_json() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let name = |text: String| text.parse::<ProviderName>().unwrap();
        let files = |provider: String| Files::new(temp.path().to_path_buf(), &name(provider));
        assert!(files("a".repeat(MAX_NAME_BYTES)).is_ok());
        assert!(files("a".repeat(MAX_NAME_BYTES + 1)).is_err());
        assert!(files("a\0b".into()).is_err());
        let files = files("test".into()).unwrap();
        let path = files.store.clone();
        let unrelated = temp.path().join("unrelated");
        fs::write(&unrelated, b"DO-NOT-READ").unwrap();
        symlink(&unrelated, &path).unwrap();
        assert!(read_store(&files).is_err());
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"TOP-SECRET invalid JSON").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_store(&files).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let err = read_store(&files).err().unwrap();
        assert!(!format!("{err:?}").contains("TOP-SECRET"));
    }

    /// An existing directory must already be private; it is never chmodded.
    #[test]
    fn credential_directory_must_be_private() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let mode = |mode| fs::set_permissions(temp.path(), fs::Permissions::from_mode(mode));
        assert!(!private_directory(&temp.path().join("absent")).unwrap());
        mode(0o700).unwrap();
        assert!(private_directory(temp.path()).unwrap());
        for permissive in [0o750, 0o705] {
            mode(permissive).unwrap();
            assert!(private_directory(temp.path()).is_err());
            assert!(create_directory(temp.path()).is_err());
            let metadata = fs::metadata(temp.path()).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o777, permissive);
        }
    }
}

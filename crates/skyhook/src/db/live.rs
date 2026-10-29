//! The files this process must not close behind SQLite's back. Closing any descriptor of a
//! file releases every POSIX record lock the process holds on it, SQLite's included: one
//! stray open and close of a live database, its `-wal` or its `-shm` lets another process
//! take SQLite's first-opener role and truncate the shared-memory index under this one.
//!
//! One registry under one lock records both sides: every database a connection holds open
//! and every file another in-process opener holds a handle to. A file opener checks and
//! registers its handle under the lock, so it never opens a live database; a database
//! registers before SQLite opens it and waits for handles on its files to close, so it
//! never becomes live under one. Replacing or removing a file decides and acts under the
//! lock. Removing a tree reserves it under the lock and removes it outside: no database
//! registers inside a reserved tree meanwhile. Closing an `O_PATH` descriptor releases no
//! locks, so lookups need no check.
//!
//! Deadlock rule: a [`Hold`] or [`Reservation`] is never dropped while its thread holds the
//! lock (handles are created last and dropped outside it), and a database open waits for handles at most
//! the busy timeout, then fails: a thread that holds a handle and opens the same database
//! gets an error, never a hang.

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs::Metadata,
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    sync::{Condvar, Mutex, MutexGuard},
    time::{Duration, Instant},
};

static REGISTRY: Mutex<State> = Mutex::new(State {
    databases: Vec::new(),
    handles: BTreeMap::new(),
    reserved: Vec::new(),
});
/// Signalled whenever a handle closes.
static RELEASED: Condvar = Condvar::new();

/// SQLite's companion files, named after the database.
const COMPANIONS: [&str; 3] = ["-wal", "-shm", "-journal"];

pub(crate) type Identity = (u64, u64);

pub(crate) fn identity(metadata: &Metadata) -> Identity {
    (metadata.dev(), metadata.ino())
}

fn parent(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// One open database: its canonical path, its directory, and how many connections hold it.
struct Database {
    path: PathBuf,
    directory: Identity,
    connections: usize,
}

impl Database {
    /// The database file and each companion, by name.
    fn names(&self) -> impl Iterator<Item = OsString> + '_ {
        let name = self.path.file_name().unwrap_or_default();
        std::iter::once(name.to_owned()).chain(COMPANIONS.iter().map(move |suffix| {
            let mut companion = name.to_owned();
            companion.push(suffix);
            companion
        }))
    }

    /// The identities of its files that exist now, which catches hard links.
    fn files(&self) -> impl Iterator<Item = Identity> + '_ {
        self.names().filter_map(|name| {
            std::fs::metadata(self.path.with_file_name(name))
                .ok()
                .map(|metadata| identity(&metadata))
        })
    }
}

pub(crate) struct State {
    databases: Vec<Database>,
    /// How many open handles each file has, by identity.
    handles: BTreeMap<Identity, usize>,
    /// The canonical roots of the trees being removed, once per removal.
    reserved: Vec<PathBuf>,
}

/// A file as an opener is about to reach it: the file when it exists, and the name it
/// takes in its directory, which catches a `-wal` or `-shm` created later.
pub(crate) struct Probe {
    pub(crate) file: Option<Reached>,
    pub(crate) directory: Option<Identity>,
    pub(crate) name: Option<OsString>,
}

/// An existing file a probe reaches.
pub(crate) struct Reached {
    pub(crate) identity: Identity,
    /// Whether the file may have a name other than the probed one: it has more than one
    /// link, or a symbolic link led to it. Otherwise it is a database's file only when
    /// the probed name is.
    pub(crate) aliased: bool,
}

impl Probe {
    /// `path` as a lookup that follows links would reach it.
    pub(crate) fn path(path: &Path) -> Self {
        let file = std::fs::symlink_metadata(path).ok().and_then(|own| {
            let (metadata, linked) = if own.file_type().is_symlink() {
                (std::fs::metadata(path).ok()?, true)
            } else {
                (own, false)
            };
            Some(Reached {
                identity: identity(&metadata),
                aliased: linked || metadata.nlink() > 1,
            })
        });
        Self {
            file,
            directory: std::fs::metadata(parent(path))
                .ok()
                .map(|metadata| identity(&metadata)),
            name: path.file_name().map(OsStr::to_owned),
        }
    }
}

/// The registry, held: deciding about a file and acting on it happen under one lock.
pub(crate) struct Registry(MutexGuard<'static, State>);

pub(crate) fn lock() -> Registry {
    Registry(
        REGISTRY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    )
}

impl Registry {
    /// Whether `probe` reaches one of the open databases' files. Only a file that may have
    /// another name on a database's filesystem is compared with that database's files.
    pub(crate) fn holds(&self, probe: &Probe) -> bool {
        self.0.databases.iter().any(|database| {
            probe.directory == Some(database.directory)
                && (probe.name.as_ref())
                    .is_some_and(|name| database.names().any(|held| held == *name))
                || probe.file.as_ref().is_some_and(|file| {
                    file.aliased
                        && file.identity.0 == database.directory.0
                        && database.files().any(|held| held == file.identity)
                })
        })
    }

    /// Reserve the tree at the canonical path `tree` for its removal, unless an open
    /// database lives anywhere below it.
    pub(crate) fn reserve(mut self, tree: PathBuf) -> Option<Reservation> {
        if (self.0.databases.iter()).any(|database| database.path.starts_with(&tree)) {
            return None;
        }
        self.0.reserved.push(tree.clone());
        Some(Reservation(tree))
    }

    /// Record a handle to the file `file`, held until the returned value drops.
    pub(crate) fn hold(&mut self, file: Identity) -> Hold {
        *self.0.handles.entry(file).or_default() += 1;
        Hold(file)
    }
}

/// One open handle to a file; dropping it, outside the lock, ends the hold.
#[derive(Debug)]
pub(crate) struct Hold(Identity);

impl Drop for Hold {
    fn drop(&mut self) {
        let mut registry = lock();
        let handles = &mut registry.0.handles;
        if let Some(count) = handles.get_mut(&self.0) {
            *count -= 1;
            if *count == 0 {
                handles.remove(&self.0);
            }
        }
        drop(registry);
        RELEASED.notify_all();
    }
}

/// A tree being removed; dropping it, outside the lock, ends the reservation.
pub(crate) struct Reservation(PathBuf);

impl Drop for Reservation {
    fn drop(&mut self) {
        let reserved = &mut lock().0.reserved;
        if let Some(index) = reserved.iter().position(|tree| *tree == self.0) {
            reserved.swap_remove(index);
        }
    }
}

/// Whether a handle still holds `file` once its last one closed or `timeout` passed.
#[cfg(test)]
pub(crate) fn held_after(file: Identity, timeout: Duration) -> bool {
    let held = |state: &mut State| state.handles.contains_key(&file);
    let waited = RELEASED.wait_timeout_while(lock().0, timeout, held);
    let (mut state, _) = waited.unwrap_or_else(|poisoned| poisoned.into_inner());
    held(&mut state)
}

/// One connection's hold on its database; dropping it, after the connection closed its
/// file, ends the hold.
pub(super) struct Held(PathBuf);

impl Held {
    /// Register `path` before SQLite opens it, once no handle holds any of its files,
    /// waiting at most `timeout` for one to close. The file need not exist yet. One
    /// inside a tree being removed is refused rather than waiting, however long it has
    /// waited already: the removal takes its directory with it.
    pub(super) fn new(path: &Path, timeout: Duration) -> std::io::Result<Self> {
        // An existing file is held where it really is, so a symlink's target and the
        // companions SQLite creates beside the target are held, not the alias.
        let path = match std::fs::canonicalize(path) {
            Ok(resolved) => resolved,
            Err(_) => std::fs::canonicalize(parent(path))?
                .join(path.file_name().ok_or(std::io::ErrorKind::InvalidInput)?),
        };
        let database = Database {
            directory: identity(&std::fs::metadata(parent(&path))?),
            path: path.clone(),
            connections: 1,
        };
        let deadline = Instant::now() + timeout;
        let mut registry = lock();
        loop {
            if (registry.0.reserved.iter()).any(|tree| path.starts_with(tree)) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ResourceBusy,
                    "this database's directory is being removed",
                ));
            }
            if !(database.files()).any(|file| registry.0.handles.contains_key(&file)) {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ResourceBusy,
                    "another in-process reader holds this database's file open",
                ));
            }
            registry.0 = RELEASED
                .wait_timeout(registry.0, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        match (registry.0.databases.iter_mut()).find(|held| held.path == path) {
            Some(held) => held.connections += 1,
            None => registry.0.databases.push(database),
        }
        Ok(Self(path))
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let mut registry = lock();
        let databases = &mut registry.0.databases;
        if let Some(index) = databases.iter().position(|held| held.path == self.0) {
            databases[index].connections -= 1;
            if databases[index].connections == 0 {
                databases.swap_remove(index);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, OpenMode};

    fn held(path: &Path) -> bool {
        lock().holds(&Probe::path(path))
    }

    /// A live database's file, its companions under any later name rule and a hard link
    /// to it are held while a connection is open, and released with the last one.
    #[test]
    fn open_databases_hold_their_files_until_the_last_connection_closes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("live.db");
        let first = Db::connect(&path, OpenMode::Create).unwrap();
        first
            .batch("PRAGMA journal_mode = WAL; CREATE TABLE t(x);")
            .unwrap();
        let second = Db::connect(&path, OpenMode::ReadOnly).unwrap();
        let link = root.path().join("alias");
        std::fs::hard_link(&path, &link).unwrap();
        let unrelated = root.path().join("other.db");
        std::fs::write(&unrelated, b"").unwrap();
        for file in [
            "live.db",
            "live.db-wal",
            "live.db-shm",
            "live.db-journal",
            "alias",
        ] {
            assert!(held(&root.path().join(file)), "{file}");
        }
        assert!(!held(&unrelated));
        // Opened through a symlink, the database's real files are held.
        let target = root.path().join("target.db");
        drop(Db::connect(&target, OpenMode::Create).unwrap());
        let symlink = root.path().join("symlink.db");
        std::os::unix::fs::symlink(&target, &symlink).unwrap();
        let through = Db::connect(&symlink, OpenMode::Open).unwrap();
        through
            .batch("PRAGMA journal_mode = WAL; CREATE TABLE t(x);")
            .unwrap();
        assert!(held(&root.path().join("target.db-wal")) && held(&symlink));
        drop(through);
        drop(first);
        assert!(held(&path));
        drop(second);
        assert!(!held(&path));
        assert!(!held(&link));
    }

    /// A database waits for every handle on its file, duplicates included, to close before
    /// it opens, so it never becomes live under one, and gives up after its timeout instead
    /// of hanging.
    #[test]
    fn a_database_open_waits_for_handles_on_its_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("closed.db");
        drop(Db::connect(&path, OpenMode::Create).unwrap());
        // A duplicate keeps the registration after the handle it came from closes.
        let opened = crate::fs::open_regular_blocking(&path, u64::MAX).unwrap();
        let file = opened.duplicate().unwrap();
        drop(opened);
        let refused = Held::new(&path, Duration::ZERO).err().unwrap();
        assert_eq!(refused.kind(), std::io::ErrorKind::ResourceBusy);
        let opener = {
            let path = path.clone();
            std::thread::spawn(move || Held::new(&path, Duration::from_secs(60)))
        };
        // The opener cannot register while the handle is open.
        assert!(!held(&path));
        drop(file);
        let held_open = opener.join().unwrap().unwrap();
        assert!(held(&path));
        drop(held_open);
    }

    /// A tree reserved for removal refuses a database inside it, but not beside it, and
    /// leaves the registry free; a tree holding a live database is not reserved.
    #[test]
    fn a_reserved_tree_refuses_databases_inside_it() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let tree = root.join("tree");
        std::fs::create_dir(&tree).unwrap();
        let reservation = lock().reserve(tree.clone()).unwrap();
        let inside = Held::new(&tree.join("inside.db"), Duration::ZERO)
            .err()
            .unwrap();
        assert_eq!(inside.kind(), std::io::ErrorKind::ResourceBusy);
        let beside = Held::new(&root.join("beside.db"), Duration::ZERO).unwrap();
        assert!(lock().reserve(root.clone()).is_none());
        drop(reservation);
        drop(Held::new(&tree.join("inside.db"), Duration::ZERO).unwrap());
        drop(beside);
    }

    /// A database already waiting for a handle to close when its tree is reserved is
    /// refused once the handle closes, rather than registering inside the removal.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_waiting_database_open_is_refused_once_its_tree_is_reserved() {
        let root = tempfile::tempdir().unwrap();
        let tree = std::fs::canonicalize(root.path()).unwrap().join("tree");
        std::fs::create_dir(&tree).unwrap();
        let path = tree.join("waiting.db");
        drop(Db::connect(&path, OpenMode::Create).unwrap());
        let file = crate::fs::open_regular_blocking(&path, u64::MAX).unwrap();
        let name = "waiting-opener";
        let opener = std::thread::Builder::new().name(name.into());
        let opener = opener
            .spawn(move || Held::new(&path, Duration::from_secs(60)))
            .unwrap();
        // Parked on the condvar: no other lock or wait in this process sleeps it.
        let asleep = || {
            let tasks = std::fs::read_dir("/proc/self/task").unwrap().flatten();
            tasks.map(|task| task.path()).any(|task| {
                let read = |file| std::fs::read_to_string(task.join(file)).unwrap_or_default();
                read("comm").trim_end() == name
                    && (read("stat").rsplit_once(") "))
                        .is_some_and(|(_, stat)| stat.starts_with('S'))
            })
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        while !asleep() {
            assert!(Instant::now() < deadline, "the opener never waited");
            std::thread::yield_now();
        }
        let reservation = lock().reserve(tree).unwrap();
        drop(file);
        let refused = opener.join().unwrap().err().unwrap();
        assert_eq!(refused.kind(), std::io::ErrorKind::ResourceBusy);
        drop(reservation);
    }
}

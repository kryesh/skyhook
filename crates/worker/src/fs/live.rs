//! The files this process must not close behind SQLite's back. Closing any descriptor of a
//! file releases every POSIX record lock the process holds on it, SQLite's included: one
//! stray open and close of a live database, its `-wal` or its `-shm` lets another process
//! take SQLite's first-opener role and truncate the shared-memory index under this one.
//! Windows locks belong to the handle that took them, so closing another releases none
//! there; the registry still keeps other openers, replacements and removals off a live
//! database's files.
//!
//! One registry under one lock records both sides: every database a connection holds open
//! and every file another in-process opener holds a handle to. A file opener checks the
//! file its name reaches and holds it under the lock, then opens outside it; a database
//! registers before SQLite opens it and waits for handles on its files to close, so it
//! never becomes live under one. A descriptor that reached another file by the time it
//! opened is decided about under the lock instead, and one that turns out to be a live
//! database's is parked on that database until its last connection closes: closing it
//! sooner would release SQLite's locks. Replacing or removing a file decides and acts
//! under the lock. Removing a tree reserves it under the lock and removes it outside: no database
//! registers inside a reserved tree meanwhile.
//!
//! Deadlock rule: a [`Hold`] or [`Reservation`] is never dropped while its thread holds the
//! lock (handles are created last and dropped outside it), and a database open waits for handles at most
//! the busy timeout, then fails: a thread that holds a handle and opens the same database
//! gets an error, never a hang.

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs::File,
    io,
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

/// A file's device and inode, or on Windows its volume and file index.
pub(crate) type Identity = (u64, u64);

/// The identity of the file `path` reaches, following links. On Unix this opens nothing,
/// which could release a live database's locks.
#[cfg(unix)]
fn identify(path: &Path) -> io::Result<Identity> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = std::fs::metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

/// The identity of the file `path` names and how many links it has, or none when it is
/// a symbolic link: one lookup, which does not follow it.
#[cfg(unix)]
fn own(path: &Path) -> io::Result<Option<(Identity, u64)>> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = std::fs::symlink_metadata(path)?;
    let identity = (metadata.dev(), metadata.ino());
    Ok((!metadata.is_symlink()).then_some((identity, metadata.nlink())))
}

/// The identity of the open `file`.
#[cfg(unix)]
pub(crate) fn identity(file: &File) -> io::Result<Identity> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

/// Windows tells a file's identity only through a handle. One opened without access,
/// with backup semantics, opens directories too and conflicts with no other handle.
#[cfg(windows)]
fn identify(path: &Path) -> io::Result<Identity> {
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    information(&without_access(path, FILE_FLAG_BACKUP_SEMANTICS)?).map(|found| key(&found))
}

#[cfg(windows)]
fn own(path: &Path) -> io::Result<Option<(Identity, u64)>> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };
    let flags = FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT;
    let found = information(&without_access(path, flags)?)?;
    let link = found.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    Ok((!link).then(|| (key(&found), found.nNumberOfLinks.into())))
}

#[cfg(windows)]
fn without_access(path: &Path, flags: u32) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(flags)
        .open(path)
}

#[cfg(windows)]
pub(crate) fn identity(file: &File) -> io::Result<Identity> {
    information(file).map(|information| key(&information))
}

#[cfg(windows)]
fn information(
    file: &File,
) -> io::Result<windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION> {
    use std::os::windows::io::AsRawHandle as _;
    // SAFETY: the structure is plain data, valid when zeroed.
    let mut information = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is open and the pointer is to a writable structure.
    let read = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle(
            file.as_raw_handle(),
            &raw mut information,
        )
    };
    if read == windows_sys::Win32::Foundation::FALSE {
        return Err(io::Error::last_os_error());
    }
    Ok(information)
}

#[cfg(windows)]
fn key(
    information: &windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION,
) -> Identity {
    let index = u64::from(information.nFileIndexHigh) << 32 | u64::from(information.nFileIndexLow);
    (information.dwVolumeSerialNumber.into(), index)
}

fn parent(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// One open database: its canonical path, its directory, how many connections hold it,
/// and the descriptors of its files parked until the last one closes.
struct Database {
    path: PathBuf,
    directory: Identity,
    connections: usize,
    parked: Vec<std::fs::File>,
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

    /// Whether `file` is one of its files now, under any name.
    fn owns(&self, file: Identity) -> bool {
        file.0 == self.directory.0 && self.files().any(|held| held == file)
    }

    /// The identities of its files that exist now, which catches hard links.
    fn files(&self) -> impl Iterator<Item = Identity> + '_ {
        (self.names()).filter_map(|name| identify(&self.path.with_file_name(name)).ok())
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
    /// `path` as a lookup that follows links would reach it. Whether the file has another
    /// name comes from the lookup that found it, so a rename in between cannot pair one
    /// file's links with another's identity; whatever a symbolic link reaches may.
    pub(crate) fn path(path: &Path) -> Self {
        let file = match own(path) {
            Ok(Some((identity, links))) => Some(Reached {
                identity,
                aliased: links > 1,
            }),
            Ok(None) => identify(path).ok().map(|identity| Reached {
                identity,
                aliased: true,
            }),
            Err(_) => None,
        };
        Self {
            file,
            directory: identify(parent(path)).ok(),
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
    /// Whether `probe` reaches one of the open databases' files.
    pub(crate) fn holds(&self, probe: &Probe) -> bool {
        self.reached(probe).is_some()
    }

    /// The open database whose file `probe` reaches. Only a file that may have another
    /// name on a database's filesystem is compared with that database's files.
    fn reached(&self, probe: &Probe) -> Option<usize> {
        self.0.databases.iter().position(|database| {
            probe.directory == Some(database.directory)
                && (probe.name.as_ref())
                    .is_some_and(|name| database.names().any(|held| held == *name))
                || (probe.file.as_ref())
                    .is_some_and(|file| file.aliased && database.owns(file.identity))
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

    /// Keep `file`, a descriptor of the file `probe` reaches, open until the database
    /// whose file it is closes. With the file's identity known, only the database owning
    /// it qualifies, so one that took the probed name meanwhile never takes over another
    /// file's lifetime; a descriptor no open database owns is closed now, under the lock.
    pub(crate) fn park(&mut self, probe: &Probe, file: std::fs::File) {
        let owner = match &probe.file {
            Some(reached) => {
                (self.0.databases.iter()).position(|database| database.owns(reached.identity))
            }
            None => self.reached(probe),
        };
        if let Some(index) = owner {
            self.0.databases[index].parked.push(file);
        }
    }
}

/// One open handle to a file; dropping it, outside the lock, ends the hold.
#[derive(Debug)]
pub(crate) struct Hold(Identity);

impl Hold {
    pub(crate) fn file(&self) -> Identity {
        self.0
    }
}

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

/// One connection's hold on its database; dropping it, after the connection closed its
/// file, ends the hold.
pub struct Held(PathBuf);

impl Held {
    /// Register `path` before SQLite opens it, once no handle holds any of its files,
    /// waiting at most `timeout` for one to close. The file need not exist yet. One
    /// inside a tree being removed is refused rather than waiting, however long it has
    /// waited already: the removal takes its directory with it.
    pub fn new(path: &Path, timeout: Duration) -> std::io::Result<Self> {
        // An existing file is held where it really is, so a symlink's target and the
        // companions SQLite creates beside the target are held, not the alias.
        let path = match std::fs::canonicalize(path) {
            Ok(resolved) => resolved,
            Err(_) => std::fs::canonicalize(parent(path))?
                .join(path.file_name().ok_or(std::io::ErrorKind::InvalidInput)?),
        };
        let database = Database {
            directory: identify(parent(&path))?,
            path: path.clone(),
            connections: 1,
            parked: Vec::new(),
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
pub(crate) mod tests {
    use super::*;

    /// Whether a handle still holds `file` once its last one closed or `timeout` passed.
    pub(crate) fn held_after(file: Identity, timeout: Duration) -> bool {
        let held = |state: &mut State| state.handles.contains_key(&file);
        let waited = RELEASED.wait_timeout_while(lock().0, timeout, held);
        let (mut state, _) = waited.unwrap_or_else(|poisoned| poisoned.into_inner());
        held(&mut state)
    }

    /// A connection to the database at `path`, as one registers.
    fn connect(path: &Path) -> Held {
        Held::new(path, Duration::ZERO).unwrap()
    }

    fn held(path: &Path) -> bool {
        lock().holds(&Probe::path(path))
    }

    /// A live database's file, its companions under any later name rule and a hard link
    /// to it are held while a connection is open, and released with the last one.
    #[test]
    fn open_databases_hold_their_files_until_the_last_connection_closes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("live.db");
        std::fs::write(&path, b"").unwrap();
        let first = connect(&path);
        let second = connect(&path);
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
        std::fs::write(&target, b"").unwrap();
        let symlink = root.path().join("symlink.db");
        std::os::unix::fs::symlink(&target, &symlink).unwrap();
        let through = connect(&symlink);
        assert!(held(&root.path().join("target.db-wal")) && held(&symlink));
        drop(through);
        drop(first);
        assert!(held(&path));
        drop(second);
        assert!(!held(&path));
        assert!(!held(&link));
    }

    /// A name swapped back and forth between a plain file and a symbolic link to a live
    /// database is held whenever a probe of it reaches the database.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_name_renamed_onto_a_link_to_a_database_is_held_whenever_it_reaches_it() {
        use std::os::unix::ffi::OsStrExt as _;
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("live.db");
        std::fs::write(&database, b"").unwrap();
        let _held = connect(&database);
        let (name, swap) = (root.path().join("name"), root.path().join("swap"));
        std::fs::write(&name, b"").unwrap();
        std::os::unix::fs::symlink(&database, &swap).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let swapper = {
            let text = |path: &Path| std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            let (name, swap, stop) = (text(&name), text(&swap), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    // SAFETY: both paths are NUL-terminated and live across the call.
                    let swapped = unsafe {
                        libc::renameat2(
                            libc::AT_FDCWD,
                            name.as_ptr(),
                            libc::AT_FDCWD,
                            swap.as_ptr(),
                            libc::RENAME_EXCHANGE,
                        )
                    };
                    assert_eq!(swapped, 0, "{}", io::Error::last_os_error());
                }
            })
        };
        let live = identify(&database).unwrap();
        let unheld = (0..100_000)
            .map(|_| Probe::path(&name))
            .filter(|probe| matches!(&probe.file, Some(file) if file.identity == live))
            .filter(|probe| !lock().holds(probe))
            .count();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        swapper.join().unwrap();
        assert_eq!(unheld, 0);
    }

    /// A database waits for every handle on its file, duplicates included, to close before
    /// it opens, so it never becomes live under one, and gives up after its timeout instead
    /// of hanging.
    #[test]
    fn a_database_open_waits_for_handles_on_its_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("closed.db");
        std::fs::write(&path, b"").unwrap();
        let file = crate::fs::open_regular_blocking(&path, u64::MAX).unwrap();
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
        std::fs::write(&path, b"").unwrap();
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

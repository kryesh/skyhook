//! Directories held by descriptor, and files opened below them without following links.

use std::{
    ffi::{OsStr, OsString},
    fs, io,
    path::Path,
};

use super::{
    RegularFileError,
    guarded::{OpenFile, admit_regular, open_guarded},
};

/// A directory handle usable only as a lookup base, so opening one needs search
/// permission rather than read permission.
const SEARCH_ONLY: libc::c_int = cfg_select! {
    any(target_os = "linux", target_os = "android") => libc::O_PATH,
    _ => 0,
};

/// Why a file confined below a directory could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum BeneathError {
    #[error("symbolic links are not followed")]
    Symlink,
    #[error(transparent)]
    File(#[from] RegularFileError),
}

fn invalid_name() -> BeneathError {
    BeneathError::File(io::Error::from(io::ErrorKind::InvalidInput).into())
}

fn c_name(name: &OsStr) -> Result<std::ffi::CString, BeneathError> {
    use std::os::unix::ffi::OsStrExt as _;
    std::ffi::CString::new(name.as_bytes()).map_err(|_| invalid_name())
}

fn os_error() -> BeneathError {
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ELOOP) => BeneathError::Symlink,
        Some(libc::ENOTDIR) => RegularFileError::NotRegular.into(),
        _ => RegularFileError::Io(error).into(),
    }
}

/// Open `name` relative to `at` without following a symbolic link there.
fn open_at(
    at: libc::c_int,
    name: &OsStr,
    flags: libc::c_int,
    mode: libc::c_uint,
) -> Result<fs::File, BeneathError> {
    use std::os::fd::FromRawFd as _;
    let name = c_name(name)?;
    // SAFETY: `name` is NUL-terminated and `at` is AT_FDCWD or a descriptor the caller
    // keeps open for the call.
    let fd = unsafe {
        libc::openat(
            at,
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    };
    if fd < 0 {
        let error = os_error();
        // A directory open reports a link as ENOTDIR, not ELOOP.
        return Err(match error {
            BeneathError::File(RegularFileError::NotRegular) if is_link(at, &name) => {
                BeneathError::Symlink
            }
            error => error,
        });
    }
    // SAFETY: `fd` was just opened and nothing else owns it.
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

/// The descriptor of `root`'s parent and `root`'s final component. `root` must end in a
/// plain component, however it is spelled; its ancestors are trusted and resolved
/// normally, and on Linux its parent needs only search permission.
fn split_root(root: &Path) -> Result<(Option<fs::File>, &OsStr), BeneathError> {
    use std::{os::unix::fs::OpenOptionsExt as _, path::Component};
    let mut ancestors = root.components();
    let Some(Component::Normal(entry)) = ancestors.next_back() else {
        return Err(invalid_name());
    };
    let parent = ancestors.as_path();
    if parent.as_os_str().is_empty() {
        return Ok((None, entry));
    }
    let parent = fs::OpenOptions::new()
        .read(true)
        .custom_flags(SEARCH_ONLY | libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(parent)
        .map_err(RegularFileError::Io)?;
    Ok((Some(parent), entry))
}

fn is_link(at: libc::c_int, name: &std::ffi::CStr) -> bool {
    let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `name` is NUL-terminated, `at` is valid for the call, and `status` is
    // written before it is read.
    let found = unsafe {
        libc::fstatat(
            at,
            name.as_ptr(),
            status.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == 0;
    // SAFETY: fstatat succeeded, so it filled `status`.
    found && unsafe { status.assume_init() }.st_mode & libc::S_IFMT == libc::S_IFLNK
}

fn descriptor(file: Option<&fs::File>) -> libc::c_int {
    use std::os::fd::AsRawFd as _;
    file.map_or(libc::AT_FDCWD, |file| file.as_raw_fd())
}

/// Open `relative` below `root` like [`open_regular_blocking`], without following a
/// symbolic link at `root` or any component below it. Each component is opened
/// relative to its parent's descriptor, so a link swapped in after a check is never
/// followed. `root` is spelled as for [`Directory::open_or_create`]; `relative` must
/// consist of plain components.
pub fn open_beneath_blocking(
    root: &Path,
    relative: &Path,
    limit: u64,
) -> Result<OpenFile, BeneathError> {
    use std::{os::fd::AsRawFd as _, path::Component};
    let (parent, entry) = split_root(root)?;
    let read = |at, name, flags| open_at(at, name, flags | libc::O_RDONLY, 0);
    let mut directory = read(descriptor(parent.as_ref()), entry, libc::O_DIRECTORY)?;
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(invalid_name());
        };
        if components.peek().is_none() {
            let probe = probe_at(&directory, name)?;
            let (file, metadata) = open_guarded(&probe, || {
                read(directory.as_raw_fd(), name, libc::O_NONBLOCK)
            })?;
            admit_regular(metadata, limit)?;
            return Ok(file);
        }
        directory = read(directory.as_raw_fd(), name, libc::O_DIRECTORY)?;
    }
    Err(invalid_name())
}

/// `name` below `directory` as [`crate::db::live::Probe`] describes it, without
/// following a link there.
fn probe_at(directory: &fs::File, name: &OsStr) -> Result<crate::db::live::Probe, BeneathError> {
    use std::os::fd::AsRawFd as _;
    let c = c_name(name)?;
    let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `c` is NUL-terminated, the descriptor is open for the call, and `status`
    // is read only when fstatat filled it.
    let file = (unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            c.as_ptr(),
            status.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == 0)
        .then(|| {
            // SAFETY: fstatat succeeded, so it filled `status`.
            let status = unsafe { status.assume_init() };
            crate::db::live::Reached {
                identity: (status.st_dev, status.st_ino),
                aliased: status.st_nlink > 1,
            }
        });
    let directory = directory.metadata().map_err(RegularFileError::Io)?;
    Ok(crate::db::live::Probe {
        file,
        directory: Some(crate::db::live::identity(&directory)),
        name: Some(name.to_owned()),
    })
}

/// A directory held by descriptor: every name below it resolves relative to that
/// descriptor and never through a symbolic link, so a link swapped in for a directory
/// after it was opened cannot redirect a write.
pub struct Directory(fs::File);

impl Directory {
    /// Open `root`, creating it when missing, without following a link at its final
    /// component; its entry is durable in its parent before it returns. The parent is
    /// synced even when `root` exists, since an earlier call may have created it and
    /// failed before syncing. `root` must end in a plain component; its ancestors are
    /// trusted.
    pub fn open_or_create(root: &Path) -> Result<Self, BeneathError> {
        let (parent, entry) = split_root(root)?;
        let at = descriptor(parent.as_ref());
        match make_dir(at, entry) {
            Ok(()) => {}
            Err(BeneathError::File(RegularFileError::Io(error)))
                if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let parent = root
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
        fs::File::open(parent.unwrap_or(Path::new(".")))
            .and_then(|parent| parent.sync_all())
            .map_err(RegularFileError::Io)?;
        open_at(at, entry, libc::O_RDONLY | libc::O_DIRECTORY, 0).map(Self)
    }

    /// Create the directory `name` below this one and open it. An existing entry of
    /// any kind is reported as `AlreadyExists` and left untouched. Its entry is durable
    /// once this directory is synced.
    pub fn create_dir(&self, name: &OsStr) -> Result<Self, BeneathError> {
        use std::os::fd::AsRawFd as _;
        make_dir(self.0.as_raw_fd(), name)?;
        open_at(
            self.0.as_raw_fd(),
            name,
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        )
        .map(Self)
    }

    /// Write a new private file `name` below this directory from `contents`, in chunks,
    /// and sync it; an existing entry is never replaced. A file it created but could not
    /// finish is removed again. Its entry is durable once this directory is synced.
    pub fn write_new(&self, name: &OsStr, mut contents: impl io::Read) -> Result<(), BeneathError> {
        use std::os::fd::AsRawFd as _;
        let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL;
        let mut file = open_at(self.0.as_raw_fd(), name, flags, 0o600)?;
        let written = io::copy(&mut contents, &mut file).and_then(|_| file.sync_all());
        if let Err(error) = written {
            drop(file);
            let _ = self.remove(name, Entry::File);
            return Err(RegularFileError::Io(error).into());
        }
        Ok(())
    }

    /// Remove the entry `name` below this directory: a file or link, or an empty directory.
    fn remove(&self, name: &OsStr, entry: Entry) -> io::Result<()> {
        use std::os::fd::AsRawFd as _;
        let name = c_name(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let flags = match entry {
            Entry::File => 0,
            Entry::Directory => libc::AT_REMOVEDIR,
        };
        // SAFETY: `name` is NUL-terminated and the descriptor is owned by `self`.
        match unsafe { libc::unlinkat(self.0.as_raw_fd(), name.as_ptr(), flags) } {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }

    pub fn sync(&self) -> io::Result<()> {
        self.0.sync_all()
    }

    /// Open the existing directory `name` below this one, without following a link there.
    pub fn open_dir(&self, name: &OsStr) -> Result<Self, BeneathError> {
        use std::os::fd::AsRawFd as _;
        open_at(
            self.0.as_raw_fd(),
            name,
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        )
        .map(Self)
    }

    /// Open the regular file `name` below this directory for reading, without following a
    /// link there.
    pub fn open_file(&self, name: &OsStr) -> Result<fs::File, BeneathError> {
        use std::os::fd::AsRawFd as _;
        let file = open_at(
            self.0.as_raw_fd(),
            name,
            libc::O_RDONLY | libc::O_NONBLOCK,
            0,
        )?;
        admit_regular(file.metadata().map_err(RegularFileError::Io)?, u64::MAX)?;
        Ok(file)
    }

    /// The names of this directory's entries, without `.` and `..`.
    pub fn entries(&self) -> io::Result<Vec<OsString>> {
        use std::os::{fd::AsRawFd as _, unix::ffi::OsStrExt as _};
        // SAFETY: the duplicate is a new descriptor that `fdopendir` takes over.
        let fd = unsafe { libc::dup(self.0.as_raw_fd()) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is an open directory descriptor nothing else owns.
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            // SAFETY: `fdopendir` failed, so `fd` is still ours to close.
            unsafe { libc::close(fd) };
            return Err(error);
        }
        // The duplicate shares this descriptor's offset, which an earlier listing moved.
        // SAFETY: `stream` is open until `closedir` below.
        unsafe { libc::rewinddir(stream) };
        let mut names = Vec::new();
        loop {
            // SAFETY: `stream` is open; the entry is read before the next call.
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                break;
            }
            // SAFETY: `d_name` is NUL-terminated within the entry.
            let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name != b"." && name != b".." {
                names.push(OsStr::from_bytes(name).to_owned());
            }
        }
        // SAFETY: `stream` is open and closed once, which closes `fd`.
        unsafe { libc::closedir(stream) };
        Ok(names)
    }

    /// Remove the entry `name` below this directory with everything below it; a link is
    /// removed, never followed.
    pub fn remove_tree(&self, name: &OsStr) -> io::Result<()> {
        match self.open_dir(name) {
            Ok(directory) => {
                for entry in directory.entries()? {
                    directory.remove_tree(&entry)?;
                }
                self.remove(name, Entry::Directory)
            }
            Err(BeneathError::Symlink | BeneathError::File(RegularFileError::NotRegular)) => {
                self.remove(name, Entry::File)
            }
            Err(BeneathError::File(RegularFileError::Io(error))) => Err(error),
            Err(error) => Err(io::Error::other(error)),
        }
    }

    /// Move the entry `from` below this directory to `to` beside it, never replacing an
    /// existing entry there.
    pub fn rename(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
        rename_at(&self.0, from, to, Rename::NoReplace)
    }

    /// Swap the entries `a` and `b` below this directory in one step.
    pub fn exchange(&self, a: &OsStr, b: &OsStr) -> io::Result<()> {
        rename_at(&self.0, a, b, Rename::Exchange)
    }

    /// Replace `name` below this directory, in one step, with a new private file written
    /// from `contents`, staged under a hidden name beside it and synced first.
    pub fn replace(&self, name: &OsStr, contents: impl io::Read) -> Result<(), BeneathError> {
        use std::os::fd::AsRawFd as _;
        let staging = staging_name(name).map_err(RegularFileError::Io)?;
        self.write_new(&staging, contents)?;
        let (from, to) = (c_name(&staging)?, c_name(name)?);
        // SAFETY: both names are NUL-terminated and the descriptor is owned by `self`.
        let renamed = unsafe {
            libc::renameat(
                self.0.as_raw_fd(),
                from.as_ptr(),
                self.0.as_raw_fd(),
                to.as_ptr(),
            )
        };
        if renamed != 0 {
            let error = io::Error::last_os_error();
            let _ = self.remove(&staging, Entry::File);
            return Err(RegularFileError::Io(error).into());
        }
        Ok(())
    }
}

/// A hidden name beside `name` that no other staging of it takes: `.{name}.{nonce}.tmp`.
pub fn staging_name(name: &OsStr) -> io::Result<OsString> {
    let mut nonce = [0; 8];
    getrandom::fill(&mut nonce).map_err(io::Error::other)?;
    let mut staging = OsString::from(".");
    staging.push(name);
    staging.push(format!(".{:016x}.tmp", u64::from_ne_bytes(nonce)));
    Ok(staging)
}

#[derive(Clone, Copy)]
enum Entry {
    File,
    Directory,
}

#[derive(Clone, Copy)]
enum Rename {
    NoReplace,
    Exchange,
}

/// Move `from` to `to`, both below `directory`.
fn rename_at(directory: &fs::File, from: &OsStr, to: &OsStr, mode: Rename) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;
    let invalid = |_| io::Error::from(io::ErrorKind::InvalidInput);
    let (from, to) = (c_name(from).map_err(invalid)?, c_name(to).map_err(invalid)?);
    let at = directory.as_raw_fd();
    // SAFETY: both names are NUL-terminated and the descriptor is open for the call.
    let renamed = cfg_select! {
        any(target_os = "linux", target_os = "android") => unsafe {
            libc::renameat2(
                at,
                from.as_ptr(),
                at,
                to.as_ptr(),
                match mode {
                    Rename::NoReplace => libc::RENAME_NOREPLACE,
                    Rename::Exchange => libc::RENAME_EXCHANGE,
                },
            )
        },
        target_vendor = "apple" => unsafe {
            libc::renameatx_np(
                at,
                from.as_ptr(),
                at,
                to.as_ptr(),
                match mode {
                    Rename::NoReplace => libc::RENAME_EXCL,
                    Rename::Exchange => libc::RENAME_SWAP,
                },
            )
        },
        _ => {{
            let _ = (at, &from, &to, mode);
            return Err(io::ErrorKind::Unsupported.into());
        }}
    };
    match renamed {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

fn make_dir(at: libc::c_int, name: &OsStr) -> Result<(), BeneathError> {
    let name = c_name(name)?;
    // SAFETY: `name` is NUL-terminated and `at` is AT_FDCWD or a descriptor the caller
    // keeps open for the call.
    match unsafe { libc::mkdirat(at, name.as_ptr(), 0o700) } {
        0 => Ok(()),
        _ => Err(RegularFileError::Io(io::Error::last_os_error()).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Directory work below a held descriptor: a link is removed, never followed, an exchange
    /// swaps two trees in one step, a rename never replaces, and a replacement leaves no
    /// staging file.
    #[test]
    fn directory_trees_are_swapped_replaced_and_removed_beneath_their_descriptor() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("kept"), b"outside").unwrap();
        let reports = Directory::open_or_create(&root.path().join("reports")).unwrap();
        for (name, body) in [("old", b"old" as &[u8]), ("new", b"new")] {
            let tree = reports.create_dir(name.as_ref()).unwrap();
            tree.create_dir("nested".as_ref())
                .unwrap()
                .write_new("file".as_ref(), body)
                .unwrap();
        }
        std::os::unix::fs::symlink(outside.path(), root.path().join("reports/old/link")).unwrap();
        reports.exchange("old".as_ref(), "new".as_ref()).unwrap();
        let read = |path: &str| fs::read(root.path().join("reports").join(path)).unwrap();
        assert_eq!(read("old/nested/file"), b"new");
        assert!(matches!(
            reports
                .open_dir("new".as_ref())
                .unwrap()
                .open_dir("link".as_ref()),
            Err(BeneathError::Symlink)
        ));
        let refused = reports.rename("new".as_ref(), "old".as_ref());
        assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        reports.remove_tree("new".as_ref()).unwrap();
        assert_eq!(fs::read(outside.path().join("kept")).unwrap(), b"outside");
        reports
            .replace("index".as_ref(), b"first" as &[u8])
            .unwrap();
        reports
            .replace("index".as_ref(), b"second" as &[u8])
            .unwrap();
        assert_eq!(read("index"), b"second");
        let mut names = reports.entries().unwrap();
        names.sort();
        assert_eq!(names, ["index", "old"]);
    }
}

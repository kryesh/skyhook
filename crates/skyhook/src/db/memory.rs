//! In-memory databases filled from a schema image. libsql exposes no connection
//! handle, so a raw connection copies the image into the named in-memory database
//! libsql's connection holds.

use std::{
    ffi::{CStr, CString},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use libsql::{OpenFlags, ffi};

use super::{Db, DbError, DbResult};

/// A new in-memory database holding `image`.
pub(super) fn filled(image: &'static [u8]) -> DbResult<Db> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    // The memdb VFS shares a database among this process's connections by a name
    // starting with "/", and frees it when the last one closes.
    let name = format!(
        "file:/skyhook-{}?vfs=memdb",
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    // libsql's connection comes first: it configures SQLite before its first use.
    let db = Db::at(
        Path::new(&name),
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        None,
    )?;
    let source = Raw::open(":memory:")?;
    source.deserialize(image)?;
    Raw::open(&name)?.restore(&source)?;
    Ok(db)
}

/// A connection of SQLite's own C API, closed when dropped.
struct Raw(*mut ffi::sqlite3);

impl Raw {
    fn open(name: &str) -> DbResult<Self> {
        let name = CString::new(name).expect("names hold no NUL");
        let mut db = std::ptr::null_mut();
        // SAFETY: `name` is a C string and `db` receives a handle this value owns,
        // even when the open fails.
        let rc = unsafe {
            ffi::sqlite3_open_v2(
                name.as_ptr(),
                &mut db,
                ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE | ffi::SQLITE_OPEN_URI,
                std::ptr::null(),
            )
        };
        let raw = Self(db);
        raw.check(rc)?;
        Ok(raw)
    }

    /// Make the main database a read-only view of `image`, which outlives it.
    fn deserialize(&self, image: &'static [u8]) -> DbResult<()> {
        let size = image.len() as i64;
        // SAFETY: SQLite neither writes nor frees a read-only buffer it was not given
        // to own, and the buffer is static.
        let rc = unsafe {
            ffi::sqlite3_deserialize(
                self.0,
                c"main".as_ptr(),
                image.as_ptr().cast_mut(),
                size,
                size,
                ffi::SQLITE_DESERIALIZE_READONLY as u32,
            )
        };
        self.check(rc)
    }

    /// Replace the main database's pages with `source`'s.
    fn restore(&self, source: &Raw) -> DbResult<()> {
        // SAFETY: both handles are open for the backup's whole life, which ends here.
        let rc = unsafe {
            let backup =
                ffi::sqlite3_backup_init(self.0, c"main".as_ptr(), source.0, c"main".as_ptr());
            if backup.is_null() {
                ffi::sqlite3_errcode(self.0)
            } else {
                match ffi::sqlite3_backup_step(backup, -1) {
                    ffi::SQLITE_DONE => ffi::sqlite3_backup_finish(backup),
                    failed => {
                        ffi::sqlite3_backup_finish(backup);
                        failed
                    }
                }
            }
        };
        self.check(rc)
    }

    fn check(&self, rc: i32) -> DbResult<()> {
        if rc == ffi::SQLITE_OK {
            return Ok(());
        }
        // SAFETY: an open handle, or null, for which SQLite reports out of memory.
        let message = unsafe { CStr::from_ptr(ffi::sqlite3_errmsg(self.0)) };
        Err(DbError::Sql(libsql::Error::SqliteFailure(
            rc,
            message.to_string_lossy().into_owned(),
        )))
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        // SAFETY: the handle is this value's alone; closing null is a no-op.
        unsafe { ffi::sqlite3_close(self.0) };
    }
}

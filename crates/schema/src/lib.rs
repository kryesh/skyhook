//! The session journal's schema image, built at compile time: an SQLite database holding
//! only the schema, serialized, which creating a session copies instead of running the
//! DDL. SQLite's file format is platform-independent, so an image built on the host
//! serves every platform.

use std::ffi::{CStr, CString, c_char};

use libsql::ffi;
use proc_macro2::Literal;
use quote::quote;

/// The session journal's schema.
const JOURNAL: &str = include_str!("session.sql");

/// The journal's schema image, as `&[u8]`: the bytes every session is created from.
#[proc_macro]
pub fn image(_input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    match build(JOURNAL) {
        Ok(image) => {
            let image = Literal::byte_string(&image);
            quote!(#image)
        }
        Err(message) => quote!(compile_error!(#message)),
    }
    .into()
}

/// An empty in-memory database after `ddl`, serialized.
fn build(ddl: &str) -> Result<Vec<u8>, String> {
    let db = Connection::open()?;
    db.execute(ddl)
        .map_err(|message| format!("the session journal's schema: {message}"))?;
    db.serialize()
}

/// A connection of the SQLite libsql builds, closed when dropped.
struct Connection(*mut ffi::sqlite3);

impl Connection {
    fn open() -> Result<Self, String> {
        let mut db = std::ptr::null_mut();
        // SAFETY: `db` receives a handle this value owns, even when the open fails.
        let rc = unsafe {
            ffi::sqlite3_open_v2(
                c":memory:".as_ptr(),
                &mut db,
                ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE,
                std::ptr::null(),
            )
        };
        let connection = Self(db);
        match rc {
            ffi::SQLITE_OK => Ok(connection),
            _ => Err(connection.message()),
        }
    }

    fn execute(&self, ddl: &str) -> Result<(), String> {
        let ddl = CString::new(ddl).map_err(|_| "the DDL holds a NUL".to_owned())?;
        let mut error: *mut c_char = std::ptr::null_mut();
        // SAFETY: an open handle and a C string; SQLite allocates any error message,
        // which is freed after copying it out.
        unsafe {
            match ffi::sqlite3_exec(self.0, ddl.as_ptr(), None, std::ptr::null_mut(), &mut error) {
                ffi::SQLITE_OK => Ok(()),
                _ if error.is_null() => Err(self.message()),
                _ => {
                    let message = CStr::from_ptr(error).to_string_lossy().into_owned();
                    ffi::sqlite3_free(error.cast());
                    Err(message)
                }
            }
        }
    }

    fn serialize(&self) -> Result<Vec<u8>, String> {
        let mut size = 0;
        // SAFETY: an open handle; SQLite allocates the copy, freed after copying it out.
        unsafe {
            let bytes = ffi::sqlite3_serialize(self.0, c"main".as_ptr(), &mut size, 0);
            if bytes.is_null() {
                return Err(self.message());
            }
            let image = std::slice::from_raw_parts(bytes, size as usize).to_vec();
            ffi::sqlite3_free(bytes.cast());
            Ok(image)
        }
    }

    fn message(&self) -> String {
        // SAFETY: an open handle, or null, for which SQLite reports out of memory.
        unsafe { CStr::from_ptr(ffi::sqlite3_errmsg(self.0)) }
            .to_string_lossy()
            .into_owned()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // SAFETY: the handle is this value's alone; closing null is a no-op.
        unsafe { ffi::sqlite3_close(self.0) };
    }
}

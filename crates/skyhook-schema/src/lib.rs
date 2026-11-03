//! Session schema images built at compile time: an SQLite database holding only a
//! schema, serialized, which creating a session copies instead of running the DDL.
//! SQLite's file format is platform-independent, so an image built on the host serves
//! every target.

use std::{
    ffi::{CStr, CString, c_char},
    path::PathBuf,
};

use libsql::ffi;
use proc_macro2::{Literal, Span, TokenStream};
use quote::quote;
use syn::{
    Ident, LitInt, LitStr, Token,
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
};

/// The session journal's schema, which every image starts from.
const JOURNAL: &str = include_str!("session.sql");

/// A product's tables layered on the session journal's schema, as a
/// `skyhook::session::ProductImage` for `skyhook::session::Schema::extended`.
///
/// `format = N` comes first: the tables' format, which the image records and every
/// open by the product checks. Then, in order, the DDL that creates the tables: each
/// `sql = "<DDL>"` or `file = "<path>"`, a path relative to the invoking crate's
/// manifest directory. A changed file rebuilds the crate. The journal's schema is
/// always beneath: `image!()` is that alone, the bytes Skyhook itself creates
/// journal-only sessions from.
///
/// Arguments must be literals: a macro sees its input unexpanded, so `concat!`,
/// `include_str!` or another macro's output cannot supply DDL.
///
/// ```ignore
/// const IMAGE: ProductImage = skyhook_schema::image!(format = 1, file = "src/schema.sql");
/// ```
#[proc_macro]
pub fn image(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let sources =
        match syn::parse::Parser::parse(Punctuated::<Source, Token![,]>::parse_terminated, input) {
            Ok(sources) => sources,
            Err(error) => return error.to_compile_error().into(),
        };
    expand(sources)
        .unwrap_or_else(|error| error.to_compile_error())
        .into()
}

/// One argument: the format, or a piece of DDL, inline or a file's.
enum Source {
    Format(LitInt),
    Sql(LitStr),
    File(LitStr),
}

impl Parse for Source {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let kind: Ident = input.parse()?;
        input.parse::<Token![=]>()?;
        match kind.to_string().as_str() {
            "format" => Ok(Self::Format(input.parse()?)),
            "sql" => Ok(Self::Sql(input.parse()?)),
            "file" => Ok(Self::File(input.parse()?)),
            _ => Err(syn::Error::new(
                kind.span(),
                "expected `format`, `sql` or `file`",
            )),
        }
    }
}

fn expand(sources: Punctuated<Source, Token![,]>) -> syn::Result<TokenStream> {
    let mut sources = sources.into_iter();
    let format = match sources.next() {
        None => return journal(),
        Some(Source::Format(format)) => format,
        Some(Source::Sql(other) | Source::File(other)) => {
            return Err(syn::Error::new(other.span(), "`format = N` comes first"));
        }
    };
    let value: i64 = format.base10_parse()?;
    let mut layers = vec![journal_layer()];
    let mut tracked = Vec::new();
    for source in sources {
        match source {
            Source::Format(format) => {
                return Err(syn::Error::new(format.span(), "one `format` only"));
            }
            Source::Sql(sql) => layers.push(("inline DDL".to_owned(), sql.value(), sql.span())),
            Source::File(file) => {
                let manifest = std::env::var_os("CARGO_MANIFEST_DIR")
                    .ok_or_else(|| syn::Error::new(file.span(), "CARGO_MANIFEST_DIR is not set"))?;
                let path = PathBuf::from(manifest).join(file.value());
                let ddl = std::fs::read_to_string(&path).map_err(|error| {
                    syn::Error::new(file.span(), format!("reading {}: {error}", path.display()))
                })?;
                let path = path.to_str().ok_or_else(|| {
                    syn::Error::new(file.span(), format!("{} is not UTF-8", path.display()))
                })?;
                tracked.push(path.to_owned());
                layers.push((path.to_owned(), ddl, file.span()));
            }
        }
    }
    if layers.len() == 1 {
        return Err(syn::Error::new(
            format.span(),
            "the tables need DDL after the format",
        ));
    }
    layers.push((
        "the format".to_owned(),
        format!(
            "CREATE TABLE extension(format INTEGER NOT NULL) STRICT; \
             INSERT INTO extension(format) VALUES ({value});"
        ),
        format.span(),
    ));
    let image = Literal::byte_string(&build(&layers)?);
    Ok(quote! {{
        #(const _: &[u8] = include_bytes!(#tracked);)*
        ::skyhook::session::ProductImage::__built(#image, #value)
    }})
}

fn journal_layer() -> (String, String, Span) {
    let name = "the session journal's schema".to_owned();
    (name, JOURNAL.to_owned(), Span::call_site())
}

fn journal() -> syn::Result<TokenStream> {
    let image = Literal::byte_string(&build(&[journal_layer()])?);
    Ok(quote! {{
        const IMAGE: &[u8] = #image;
        IMAGE
    }})
}

/// An empty in-memory database after each layer's DDL, serialized.
fn build(layers: &[(String, String, Span)]) -> syn::Result<Vec<u8>> {
    let db = Connection::open()?;
    for (name, ddl, span) in layers {
        db.execute(ddl)
            .map_err(|message| syn::Error::new(*span, format!("{name}: {message}")))?;
    }
    db.serialize()
}

/// A connection of the SQLite libsql builds, closed when dropped.
struct Connection(*mut ffi::sqlite3);

impl Connection {
    fn open() -> syn::Result<Self> {
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
            _ => Err(syn::Error::new(Span::call_site(), connection.message())),
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

    fn serialize(&self) -> syn::Result<Vec<u8>> {
        let mut size = 0;
        // SAFETY: an open handle; SQLite allocates the copy, freed after copying it out.
        unsafe {
            let bytes = ffi::sqlite3_serialize(self.0, c"main".as_ptr(), &mut size, 0);
            if bytes.is_null() {
                return Err(syn::Error::new(Span::call_site(), self.message()));
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

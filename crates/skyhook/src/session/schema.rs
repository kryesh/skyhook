//! What a session's database holds: the journal's tables, and a product's beside them.
//! Every session is created from an image whose journal schema is this build's: images
//! come only from the journal's own or from `schema::image!`, which layers a
//! product's tables on it, and each is checked before a session is created from it.

use super::db;
use crate::db::{Db, DbError, DbResult, Place, Seeding, params};

/// The tables a session holds as a host creates or opens it.
#[derive(Clone, Copy, Debug)]
pub struct Schema(Tables);

#[derive(Clone, Copy, Debug)]
enum Tables {
    Journal,
    /// A product's tables beside the journal's. They are mutable, unlike the journal's,
    /// and only the product's harness opens them: another host's harness opens the
    /// session without them.
    Product {
        image: ProductImage,
        /// Fills the tables' dictionaries in a new session, and checks them on an open.
        seed: fn(&Db, Seeding) -> DbResult<()>,
    },
}

/// A product's tables over the journal's schema, under a format of their own: any
/// change to the tables is a new one, which every open by the product checks. Made by
/// `schema::image!`, or by [`layered`](Self::layered).
#[derive(Clone, Copy, Debug)]
pub struct ProductImage {
    source: Source,
    format: i64,
}

#[derive(Clone, Copy, Debug)]
enum Source {
    /// The journal's schema with the tables and their format, serialized.
    Built(&'static [u8]),
    /// DDL run over the journal's image as each session is created.
    Layered(&'static str),
}

impl ProductImage {
    /// What `schema::image!` expands to, which built `image` and recorded
    /// `format` in it.
    #[doc(hidden)]
    pub const fn __built(image: &'static [u8], format: i64) -> Self {
        Self {
            source: Source::Built(image),
            format,
        }
    }

    /// Tables `ddl` creates over the journal's schema as each session is created, at
    /// `format`: for DDL `image!` cannot read, which only literals and files supply.
    pub const fn layered(ddl: &'static str, format: i64) -> Self {
        Self {
            source: Source::Layered(ddl),
            format,
        }
    }
}

impl Schema {
    /// The journal's tables alone.
    pub const JOURNAL: Self = Self(Tables::Journal);

    /// A product's tables from `image`, with the dictionaries `seed` fills and checks.
    pub const fn extended(image: ProductImage, seed: fn(&Db, Seeding) -> DbResult<()>) -> Self {
        Self(Tables::Product { image, seed })
    }

    /// Whether a product's tables are part of it.
    pub(super) fn has_product_tables(&self) -> bool {
        matches!(self.0, Tables::Product { .. })
    }

    /// A new session database at `place`.
    pub(super) fn create(&self, place: Place<'_>) -> DbResult<Db> {
        let Tables::Product { image, seed } = self.0 else {
            return db::create(place, db::IMAGE, |_| Ok(()));
        };
        match image.source {
            Source::Built(bytes) => db::create(place, bytes, |db| seed(db, Seeding::Fill)),
            Source::Layered(ddl) => db::create(place, db::IMAGE, |db| {
                db.batch("CREATE TABLE extension(format INTEGER NOT NULL) STRICT;")?;
                db.execute(
                    "INSERT INTO extension(format) VALUES (?1)",
                    params![image.format],
                )?;
                db.batch(ddl)?;
                seed(db, Seeding::Fill)
            }),
        }
    }

    /// Check that a session database, whose journal its opener checked, holds these
    /// tables.
    pub(super) fn check(&self, db: &Db) -> DbResult<()> {
        let Tables::Product { image, seed } = self.0 else {
            return Ok(());
        };
        // A session created without the tables has no `extension` table to read.
        let installed = db
            .query_row(
                "SELECT 1 FROM sqlite_schema WHERE type='table' AND name='extension'",
                Vec::new(),
                |_| Ok(()),
            )?
            .is_some();
        let format = match installed {
            true => db.query_row("SELECT format FROM extension", Vec::new(), |row| {
                Ok(row.get::<i64>(0)?)
            })?,
            false => None,
        };
        match format {
            Some(format) if format == image.format => seed(db, Seeding::Check),
            found => Err(DbError::Extension(found.unwrap_or_default())),
        }
    }
}

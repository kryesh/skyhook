//! Job output rows per generation. Capture chunks record the newlines before them,
//! so line seeks are indexed lookups. A capture past the saved-field limit is a file,
//! which its rows name.

use std::{io::Write as _, path::PathBuf};

use super::{
    Db, DbError, DbResult, Reads, SharedDb, SqlText, corrupt, decode::parsed, enum_column,
    optional_enum_column, params, rejected, u64_of as integer,
};
use crate::target::Revisions;
use skyhook_tools::output::{CaptureFile, CaptureKind};
use skyhook_tools::output::{FieldPointer, Presented, json::Detection};
use skyhook_worker::fs::spill::SpillFile;
use skyhook_worker::op::{SpilledStream, TargetPath};

/// Largest chunk row written; readers accept any length the schema allows.
const CHUNK_BYTES: usize = 256 * 1024;
/// Bytes of a staged capture written between syncs of the log.
const STAGED_SYNC_BYTES: usize = 4 * CHUNK_BYTES;

pub(crate) struct CaptureRow {
    pub id: i64,
    pub pointer: FieldPointer,
    /// The saved terminal document references this capture as a result field.
    pub referenced: bool,
    /// Set once a finished capture its schema declares as JSON is classified.
    pub detection: Option<Detection>,
    pub bytes: CaptureBytes,
}

/// Where a capture's bytes are.
#[derive(Clone, Debug)]
pub(crate) enum CaptureBytes {
    /// All in its chunk rows.
    Rows,
    /// All in a file the session wrote in its host's spill directory, `extent` long.
    Spilled {
        path: PathBuf,
        extent: CaptureExtent,
    },
    /// All in a file on its job's target, which wrote it or whose call named it; its
    /// chunk rows hold only their first bytes.
    Target(TargetFile),
}

/// A file on the machine a run ran on, and the route its connection reached that
/// machine along: each target on it at the revision it had then, none for the
/// session host. Only a connection along the same route reads the file.
#[derive(Clone, Debug)]
pub(crate) struct TargetFile {
    pub file: CaptureFile,
    pub route: Revisions,
}

/// Committed capture length, newline count, and whether the bytes end a line.
pub(crate) use skyhook_worker::op::Extent as CaptureExtent;

/// Insert `data` as chunk rows of `capture` from `extent`, returning the extent after.
fn write_chunks(
    db: &Db,
    capture: i64,
    mut extent: CaptureExtent,
    data: &[u8],
) -> DbResult<CaptureExtent> {
    for piece in data.chunks(CHUNK_BYTES) {
        db.execute(
            SqlText::Once(
                "INSERT INTO job_capture_chunk (capture, byte_offset, first_line, data) \
                 VALUES (?1, ?2, ?3, ?4)",
            ),
            params![capture, extent.bytes, extent.newlines, piece],
        )?;
        extent = extent.after(piece);
    }
    Ok(extent)
}

/// One run's product, prepared for the transaction that journals the run's
/// finish, which saves it once: a refused finish leaves none of it but the
/// captures staged for it, which no reader sees and the next open removes.
pub(crate) struct OutputRows {
    pub run: crate::job::Run,
    pub result: Option<String>,
    pub captures_complete: bool,
    /// Registered captures the result references, with what candidate text holds.
    pub fields: Vec<(i64, Option<Detection>)>,
    /// Parts of the result stored ahead of the finish as captures of their own.
    pub staged: Vec<StagedCapture>,
    /// Registered captures that hold the first bytes of a file on the job's target.
    pub files: Vec<(i64, CaptureFile)>,
    pub presented: Presented,
}

/// A finished capture of bytes offloaded from a result, written ahead of the finish
/// that references it, so the finish's transaction stays small. Until that commits,
/// the capture is staged: no reader lists it, and opening the session removes it.
pub(crate) struct StagedCapture(i64);

impl StagedCapture {
    pub(crate) fn id(&self) -> i64 {
        self.0
    }
}

/// Where the bytes of a capture are as it is staged.
pub(crate) enum StagedBytes<'a> {
    /// Here, to write as its chunk rows.
    Rows(&'a [u8]),
    /// In the spill file at this path already, this much of them.
    File(&'a TargetPath, CaptureExtent),
}

impl OutputRows {
    pub(in crate::session) fn insert(self, db: &Db) -> DbResult<()> {
        let Self {
            run,
            result,
            captures_complete,
            fields,
            staged,
            files,
            presented,
        } = self;
        let (job, generation) = run.key();
        db.execute(
            "INSERT INTO job_output (job, generation, captures_complete, result) \
             VALUES (?1, ?2, ?3, ?4)",
            params![job, generation, captures_complete, result],
        )?;
        let mut referenced = Vec::with_capacity(fields.len() + staged.len());
        for (capture, detection) in fields {
            if let Some(detection) = detection {
                db.execute(
                    "UPDATE job_capture SET detection = ?2 WHERE id = ?1",
                    params![capture, detection],
                )?;
            }
            referenced.push(capture);
        }
        for StagedCapture(capture) in staged {
            let published = db.execute(
                "DELETE FROM job_capture_staged WHERE capture = ?1",
                params![capture],
            )?;
            if published != 1 {
                return Err(super::rejected("an offloaded capture is no longer staged"));
            }
            referenced.push(capture);
        }
        for capture in referenced {
            db.execute(
                "INSERT INTO job_output_field (job, generation, capture) VALUES (?1, ?2, ?3)",
                params![job, generation, capture],
            )?;
        }
        for (capture, file) in files {
            match file {
                CaptureFile::Written(stream) => db.execute(
                    "INSERT INTO job_capture_written \
                       (capture, path, bytes, newlines, ends_line, tail) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        capture,
                        stream.path.as_str(),
                        stream.extent.bytes,
                        stream.extent.newlines,
                        stream.extent.ends_line,
                        stream.tail
                    ],
                )?,
                CaptureFile::Named { path, extent } => db.execute(
                    "INSERT INTO job_capture_named (capture, path, bytes, newlines, ends_line) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        capture,
                        path.as_str(),
                        extent.bytes,
                        extent.newlines,
                        extent.ends_line
                    ],
                )?,
            };
        }
        for (pointer, presentation) in presented.fields() {
            db.execute(
                "INSERT INTO job_field_presentation (job, generation, pointer, presentation) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![job, generation, pointer, presentation],
            )?;
        }
        Ok(())
    }
}

/// Remove the captures staged for finishes that never committed, with their bytes.
pub(in crate::session) fn remove_staged(db: &Db) -> DbResult<()> {
    db.execute(
        "DELETE FROM job_capture WHERE id IN (SELECT capture FROM job_capture_staged)",
        Vec::new(),
    )
    .map(drop)
}

/// One run's saved product: its result JSON, if the run produced one, and the
/// pointers of the captures it references.
pub(crate) struct SavedOutput {
    pub result: Option<String>,
    pub captures_complete: bool,
    pub fields: Vec<FieldPointer>,
}

/// The schema checks pointer spelling on insert; a row that fails here is corrupt.
fn pointer(row: &libsql::Row, index: i32) -> DbResult<FieldPointer> {
    FieldPointer::try_from(row.get::<String>(index)?).map_err(|error| corrupt(error.to_string()))
}

/// The file the session moved `capture` to, and how much of it the capture holds.
fn spilled(db: &Db, capture: i64) -> DbResult<Option<(PathBuf, CaptureExtent)>> {
    db.query_row(
        "SELECT path, bytes, newlines, ends_line FROM job_capture_spill WHERE capture = ?1",
        params![capture],
        |row| {
            let extent = CaptureExtent {
                bytes: integer(row.get(1)?),
                newlines: integer(row.get(2)?),
                ends_line: row.get(3)?,
            };
            Ok((PathBuf::from(row.get::<String>(0)?), extent))
        },
    )
}

/// The bytes a reader finds from an offset: the capture's chunk rows there, or the
/// file that holds them all.
pub(crate) enum Stored {
    Rows(Vec<Chunk>),
    File(PathBuf),
}

/// What the session holds of `capture`, its spill file or its rows: how much, and
/// what is stored of the `length` bytes from `offset`.
fn window(db: &Db, capture: i64, offset: u64, length: usize) -> DbResult<(CaptureExtent, Stored)> {
    Ok(match spilled(db, capture)? {
        Some((path, extent)) => (extent, Stored::File(path)),
        None => (
            extent(db, capture)?,
            Stored::Rows(chunks(db, capture, offset, length)?),
        ),
    })
}

/// The extent of the chunk rows `capture` has.
fn extent(db: &Db, capture: i64) -> DbResult<CaptureExtent> {
    Ok(db
        .query_row(
            "SELECT byte_offset, first_line, data FROM job_capture_chunk \
             WHERE capture = ?1 ORDER BY byte_offset DESC LIMIT 1",
            params![capture],
            |row| {
                let (bytes, newlines) = (integer(row.get(0)?), integer(row.get(1)?));
                let start = CaptureExtent {
                    bytes,
                    newlines,
                    ends_line: false,
                };
                Ok(start.after(&row.get::<Vec<u8>>(2)?))
            },
        )?
        .unwrap_or_default())
}

impl SharedDb {
    /// Reserve `pointer` in run `generation` of `job`; `None` if already reserved.
    pub(crate) fn create_capture(
        &self,
        run: crate::job::Run,
        pointer: &FieldPointer,
        kind: CaptureKind,
    ) -> Result<Option<i64>, DbError> {
        let (job, generation) = run.key();
        let created = self.write(|db| {
            db.query_row(
                "INSERT INTO job_capture (job, generation, pointer, capture_kind) \
                 VALUES (?1, ?2, ?3, ?4) ON CONFLICT (job, generation, pointer) DO NOTHING \
                 RETURNING id",
                params![job, generation, pointer, kind],
                |row| Ok(row.get::<i64>(0)?),
            )
        });
        Ok(created?)
    }

    /// Write `data` as a finished capture at `pointer` of `run`, durably, staged for
    /// the finish that will reference it. Each chunk commits on its own, so the
    /// connection is never held for more than one, and the log is synced after each
    /// `STAGED_SYNC_BYTES`, so no sync of the session's commits waits for more to
    /// reach the disk. It replaces what an abandoned preparation of the run staged
    /// at `pointer`: only the finish holding the run's claim stages for it.
    pub(crate) fn stage_capture(
        &self,
        run: crate::job::Run,
        pointer: &FieldPointer,
        (kind, detection): (CaptureKind, Option<Detection>),
        bytes: StagedBytes<'_>,
    ) -> Result<StagedCapture, DbError> {
        let (job, generation) = run.key();
        let capture = self.write(|db| {
            db.execute(
                "DELETE FROM job_capture WHERE job = ?1 AND generation = ?2 AND pointer = ?3 \
                 AND id IN (SELECT capture FROM job_capture_staged)",
                params![job, generation, pointer],
            )?;
            let capture = db.insert(
                "INSERT INTO job_capture (job, generation, pointer, capture_kind, detection) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![job, generation, pointer, kind, detection],
            )?;
            db.execute(
                "INSERT INTO job_capture_staged (capture) VALUES (?1)",
                params![capture],
            )?;
            if let StagedBytes::File(path, extent) = bytes {
                insert_spill(db, capture, path, extent)?;
            }
            Ok(capture)
        })?;
        if let StagedBytes::Rows(data) = bytes {
            let mut extent = CaptureExtent::default();
            for synced in data.chunks(STAGED_SYNC_BYTES) {
                for piece in synced.chunks(CHUNK_BYTES) {
                    extent = self.append_capture(capture, extent, piece)?;
                }
                self.sync()?;
            }
        }
        Ok(StagedCapture(capture))
    }

    /// Move `capture`'s chunk rows to `file`, an empty file at `path`, which holds the
    /// capture from then on. Only the capture's one producer writes its rows, so they
    /// are copied and synced before the transaction that records the file and removes
    /// them: every reader finds one or the other, and no write waits on the copy.
    pub(crate) fn spill_capture(
        &self,
        capture: i64,
        extent: CaptureExtent,
        (file, path): (&mut std::fs::File, &TargetPath),
    ) -> std::io::Result<()> {
        let rows = self.lock().query(
            "SELECT data FROM job_capture_chunk WHERE capture = ?1 ORDER BY byte_offset",
            params![capture],
            |row| Ok(row.get::<Vec<u8>>(0)?),
        );
        let rows = rows.map_err(std::io::Error::other)?;
        let copied =
            (rows.iter().try_for_each(|data| file.write_all(data))).and_then(|()| file.sync_data());
        copied.map_err(skyhook_worker::fs::spill::SpillError::At)?;
        let moved = self.write(|db| {
            insert_spill(db, capture, path, extent)?;
            db.execute(
                "DELETE FROM job_capture_chunk WHERE capture = ?1",
                params![capture],
            )
        });
        moved
            .map(drop)
            .map_err(|error| std::io::Error::other(DbError::from(error)))
    }

    /// Record that the file holding `capture` now holds `extent`.
    pub(crate) fn extend_spill(&self, capture: i64, extent: CaptureExtent) -> Result<(), DbError> {
        let extend = |db: &Db| {
            db.execute(
                "UPDATE job_capture_spill SET bytes = ?2, newlines = ?3, ends_line = ?4 \
                 WHERE capture = ?1",
                params![capture, extent.bytes, extent.newlines, extent.ends_line],
            )
        };
        Ok(self.write(extend).map(drop)?)
    }

    /// Remove `capture`; the file the session moved it to, if it did.
    pub(crate) fn delete_capture(&self, capture: i64) -> Result<Option<PathBuf>, DbError> {
        let delete = |db: &Db| {
            let spilled = spilled(db, capture)?;
            db.execute("DELETE FROM job_capture WHERE id = ?1", params![capture])?;
            Ok::<_, DbError>(spilled.map(|(path, _)| path))
        };
        Ok(self.write(delete)?)
    }

    /// Append `data` at `extent`, returning the new extent. Captures are not ledger
    /// state: they become durable with the session's next sync.
    pub(crate) fn append_capture(
        &self,
        capture: i64,
        extent: CaptureExtent,
        data: &[u8],
    ) -> Result<CaptureExtent, DbError> {
        if data.is_empty() {
            return Ok(extent);
        }
        Ok(self.write(|db| write_chunks(db, capture, extent, data))?)
    }

    #[cfg(test)]
    pub(crate) fn test_batch(&self, sql: &str) {
        self.lock().batch(sql).unwrap();
    }
}

/// One run's saved rows, read in one snapshot.
pub(crate) struct SavedRows {
    /// The run's product, once it finished.
    pub output: Option<SavedOutput>,
    /// Its captures, ordered by pointer, without those staged for a finish that has
    /// not committed.
    pub captures: Vec<CaptureRow>,
    pub presented: Presented,
}

/// Record where `run` keeps files of its output: on the machine its connection
/// reached along `route`, which created each of `spilled` in its spill directory.
/// A route naming a target revision the journal does not hold is refused.
pub(in crate::session) fn keep(
    db: &Db,
    run: crate::job::Run,
    route: &Revisions,
    spilled: &[SpillFile],
) -> DbResult<()> {
    let (job, generation) = run.key();
    for (position, (target, revision)) in route.iter().enumerate() {
        let journaled = db.execute(
            "INSERT INTO job_output_route (job, generation, position, revision) \
             SELECT ?1, ?2, ?3, r.id FROM target_revision r \
               JOIN target t ON t.id = r.target WHERE t.name = ?4 AND r.revision = ?5",
            params![job, generation, position, target.as_str(), *revision],
        )?;
        if journaled != 1 {
            return Err(rejected(
                "a route names a target the journal does not define",
            ));
        }
    }
    let components = (spilled.iter().enumerate()).flat_map(|(file, spilled)| {
        (spilled.components.iter().enumerate()).map(move |part| (file, part))
    });
    for (file, (position, component)) in components {
        db.execute(
            "INSERT INTO job_output_spill (job, generation, file, position, component) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![job, generation, file, position, component],
        )?;
    }
    Ok(())
}

/// The route of the connection `run`'s operation ran over, outermost target first:
/// none for a run on the session host, or one that keeps no file on its target.
fn route(db: &Db, (job, generation): (u64, i64)) -> DbResult<Revisions> {
    db.query(
        "SELECT t.name, r.revision FROM job_output_route o \
           JOIN target_revision r ON r.id = o.revision JOIN target t ON t.id = r.target \
         WHERE o.job = ?1 AND o.generation = ?2 ORDER BY o.position",
        params![job, generation],
        |row| Ok((parsed(row.get(0)?)?, integer(row.get(1)?))),
    )
}

/// The spill files the session's runs had their targets create, each with its
/// components and the route of the run that had it created.
pub(in crate::session) fn own_spills(db: &Db) -> DbResult<Vec<(Revisions, Vec<String>)>> {
    let mut files: Vec<((u64, i64, i64), Vec<String>)> = Vec::new();
    for (file, component) in db.query(
        "SELECT job, generation, file, component FROM job_output_spill \
         ORDER BY job, generation, file, position",
        Vec::new(),
        |row| {
            let file = (integer(row.get(0)?), row.get(1)?, row.get(2)?);
            Ok((file, row.get::<String>(3)?))
        },
    )? {
        match files.last_mut() {
            Some((last, components)) if *last == file => components.push(component),
            _ => files.push((file, vec![component])),
        }
    }
    (files.into_iter())
        .map(|((job, generation, _), components)| Ok((route(db, (job, generation))?, components)))
        .collect()
}

fn captures(db: &Db, key @ (job, generation): (u64, i64)) -> DbResult<Vec<CaptureRow>> {
    let route = route(db, key)?;
    let on_target = |file| {
        CaptureBytes::Target(TargetFile {
            file,
            route: route.clone(),
        })
    };
    db.query(
        "SELECT c.id, c.pointer, \
           EXISTS (SELECT 1 FROM job_output_field f WHERE f.capture = c.id), c.detection, \
           s.path, s.bytes, s.newlines, s.ends_line, \
           w.path, w.bytes, w.newlines, w.ends_line, w.tail, \
           n.path, n.bytes, n.newlines, n.ends_line \
         FROM job_capture c \
           LEFT JOIN job_capture_spill s ON s.capture = c.id \
           LEFT JOIN job_capture_written w ON w.capture = c.id \
           LEFT JOIN job_capture_named n ON n.capture = c.id \
         WHERE c.job = ?1 AND c.generation = ?2 \
           AND NOT EXISTS (SELECT 1 FROM job_capture_staged s WHERE s.capture = c.id) \
         ORDER BY c.pointer",
        params![job, generation],
        |row| {
            let text = |index| row.get::<Option<String>>(index);
            let extent = |index| {
                Ok::<_, DbError>(CaptureExtent {
                    bytes: integer(row.get(index)?),
                    newlines: integer(row.get(index + 1)?),
                    ends_line: row.get(index + 2)?,
                })
            };
            let bytes = match (text(4)?, text(8)?, text(13)?) {
                (None, None, None) => CaptureBytes::Rows,
                (Some(path), None, None) => CaptureBytes::Spilled {
                    path: path.into(),
                    extent: extent(5)?,
                },
                (None, Some(path), None) => on_target(CaptureFile::Written(SpilledStream {
                    path: TargetPath::new(path),
                    extent: extent(9)?,
                    tail: row.get(12)?,
                })),
                (None, None, Some(path)) => on_target(CaptureFile::Named {
                    path: TargetPath::new(path),
                    extent: extent(14)?,
                }),
                _ => return Err(corrupt("a capture is in more than one file")),
            };
            Ok(CaptureRow {
                id: row.get(0)?,
                pointer: pointer(row, 1)?,
                referenced: row.get::<i64>(2)? != 0,
                detection: optional_enum_column(row, 3)?,
                bytes,
            })
        },
    )
}

fn insert_spill(db: &Db, capture: i64, path: &TargetPath, extent: CaptureExtent) -> DbResult<u64> {
    db.execute(
        "INSERT INTO job_capture_spill (capture, path, bytes, newlines, ends_line) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            capture,
            path.as_str(),
            extent.bytes,
            extent.newlines,
            extent.ends_line
        ],
    )
}

pub(super) fn saved_output(
    db: &Db,
    (job, generation): (u64, i64),
) -> DbResult<Option<SavedOutput>> {
    let Some((result, captures_complete)) = db.query_row(
        "SELECT result, captures_complete FROM job_output WHERE job = ?1 AND generation = ?2",
        params![job, generation],
        |row| Ok((row.get::<Option<String>>(0)?, row.get::<bool>(1)?)),
    )?
    else {
        return Ok(None);
    };
    let fields = db.query(
        "SELECT c.pointer FROM job_output_field f JOIN job_capture c ON c.id = f.capture \
         WHERE f.job = ?1 AND f.generation = ?2 ORDER BY c.pointer",
        params![job, generation],
        |row| pointer(row, 0),
    )?;
    Ok(Some(SavedOutput {
        result,
        captures_complete,
        fields,
    }))
}

fn presented(db: &Db, (job, generation): (u64, i64)) -> DbResult<Presented> {
    let fields = db.query(
        "SELECT pointer, presentation FROM job_field_presentation \
         WHERE job = ?1 AND generation = ?2",
        params![job, generation],
        |row| Ok((pointer(row, 0)?, enum_column(row, 1)?)),
    )?;
    Ok(fields.into_iter().collect())
}

/// A capture chunk: its byte offset and bytes.
pub(crate) type Chunk = (u64, Vec<u8>);

/// The consecutive chunks overlapping the `length` bytes from `offset`.
fn chunks(db: &Db, capture: i64, offset: u64, length: usize) -> DbResult<Vec<Chunk>> {
    db.query(
        "SELECT byte_offset, data FROM job_capture_chunk WHERE capture = ?1 \
           AND byte_offset >= (SELECT coalesce(max(byte_offset), 0) FROM job_capture_chunk \
             WHERE capture = ?1 AND byte_offset <= ?2) \
           AND byte_offset < ?2 + ?3 \
         ORDER BY byte_offset",
        params![capture, offset, length as u64],
        |row| Ok((integer(row.get(0)?), row.get::<Vec<u8>>(1)?)),
    )
}

impl Reads {
    /// The rows run `run` saved: its product, captures and presented fields.
    pub(crate) fn saved(&self, run: crate::job::Run) -> Result<SavedRows, DbError> {
        let key = run.key();
        self.read(|db| {
            Ok(SavedRows {
                output: saved_output(db, key)?,
                captures: captures(db, key)?,
                presented: presented(db, key)?,
            })
        })
    }

    /// How much of `capture` the session holds, in its spill file or its rows.
    pub(crate) fn capture_extent(&self, capture: i64) -> Result<CaptureExtent, DbError> {
        self.read(|db| match spilled(db, capture)? {
            Some((_, extent)) => Ok(extent),
            None => extent(db, capture),
        })
    }

    /// A position at or before the start of one-based `line`, as its offset and line:
    /// the last chunk row starting before the line begins, or the start of a spill
    /// file, which has no index.
    pub(crate) fn capture_line(&self, capture: i64, line: u64) -> Result<(u64, u64), DbError> {
        if line <= 1 {
            return Ok((0, 1));
        }
        let chunk = self.read(|db| {
            db.query_row(
                "SELECT byte_offset, first_line FROM job_capture_chunk \
                 WHERE capture = ?1 AND first_line <= ?2 \
                 ORDER BY first_line DESC, byte_offset DESC LIMIT 1",
                params![capture, line - 2],
                |row| Ok((integer(row.get(0)?), integer(row.get::<i64>(1)?) + 1)),
            )
        })?;
        Ok(chunk.unwrap_or((0, 1)))
    }

    /// The capture's extent with what is stored of the `length` bytes from `offset`,
    /// read together.
    pub(crate) fn capture_window(
        &self,
        capture: i64,
        offset: u64,
        length: usize,
    ) -> Result<(CaptureExtent, Stored), DbError> {
        self.read(|db| window(db, capture, offset, length))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::AgentId;
    use crate::job::JobRole;
    use crate::{
        job::{JobEnd, JobTransition},
        session::{
            Command, CoreState, Plan, SessionError, SessionEvent, SessionStore,
            tests::{job_created, reopen},
        },
        tests::TestRuntime,
    };
    use skyhook_worker::job::JobId;
    use std::assert_matches;

    fn run(job: JobId, generation: u32) -> crate::job::Run {
        crate::job::Run(job, crate::job::Invocation::nth(generation))
    }

    /// Journal the finish of run `generation` of `job`, saving `result` with
    /// `captures` as its fields.
    async fn finish(
        store: &SessionStore,
        agent: &AgentId,
        (job, generation): (JobId, u32),
        result: &str,
        captures: &[i64],
    ) -> Result<(), SessionError> {
        let rows = OutputRows {
            run: run(job, generation),
            result: Some(result.into()),
            captures_complete: true,
            fields: captures.iter().map(|&capture| (capture, None)).collect(),
            staged: Vec::new(),
            files: Vec::new(),
            presented: Presented::new(),
        };
        let finished = SessionEvent::JobFinished {
            job,
            state: JobEnd::Completed,
            diagnostic: None,
            images: Vec::new(),
        };
        let entries = vec![(agent.clone(), finished)];
        let plan = |_: &CoreState| Ok::<_, SessionError>(Plan::journal(entries).saving([Ok(rows)]));
        store.commit(Command::Journal, plan).await
    }

    /// A run's output commits with its finish and references the run's own captures:
    /// a capture of another job or an earlier run refuses both, stopping the
    /// session until it is reopened. It is saved once.
    #[tokio::test]
    async fn output_commits_with_its_finish_and_is_saved_once() {
        let TestRuntime {
            root, store, agent, ..
        } = TestRuntime::on_disk().await;
        let sessions = root.path().join("sessions");
        let [first, other] = [1, 2].map(|id| JobId::new(id).unwrap());
        for job in [first, other] {
            store
                .append(agent.clone(), job_created(job, JobRole::Agent))
                .await
                .unwrap();
        }
        let field = |name| FieldPointer::result().property(name);
        let reserve = |store: &SessionStore, job: JobId, generation, name| {
            let capture = store.outputs().create_capture(
                run(job, generation),
                &field(name),
                CaptureKind::Text,
            );
            capture.unwrap().unwrap()
        };
        let saved = |store: &SessionStore, generation| {
            let output = store
                .test_reads()
                .saved(run(first, generation))
                .unwrap()
                .output;
            output.map(|output| {
                assert_eq!(output.result.as_deref(), Some("{}"));
                output.fields
            })
        };
        let refused = async |store: SessionStore, generation, capture| {
            let records = store.records();
            let finish = finish(&store, &agent, (first, generation), "{}", &[capture]).await;
            assert_matches!(finish, Err(SessionError::JournalFailed(_)));
            assert_eq!(store.records(), records);
            assert_eq!(saved(&store, generation), None);
            assert!(store.close().await.is_err());
            reopen(&sessions, store.id()).await
        };
        let earlier = reserve(&store, first, 0, "earlier");
        let foreign = reserve(&store, other, 0, "foreign");
        let store = refused(store, 0, foreign).await;
        finish(&store, &agent, (first, 0), "{}", &[earlier])
            .await
            .unwrap();
        assert_eq!(saved(&store, 0), Some(vec![field("earlier")]));

        let restart = SessionEvent::JobStateChanged {
            job: first,
            state: JobTransition::Running,
        };
        store.append(agent.clone(), restart).await.unwrap();
        assert_eq!(saved(&store, 1), None);
        let store = refused(store, 1, earlier).await;
        let current = reserve(&store, first, 1, "current");
        finish(&store, &agent, (first, 1), "{}", &[current])
            .await
            .unwrap();
        assert_eq!(saved(&store, 1), Some(vec![field("current")]));
    }

    /// Bytes offloaded from a result are written ahead of its finish: until the finish
    /// commits, no reader lists them, and a session that stops before it reopens
    /// without them, its pointer free for the run's finish.
    #[tokio::test]
    async fn offloaded_bytes_whose_finish_never_committed_are_gone_on_reopen() {
        let TestRuntime {
            root, store, agent, ..
        } = TestRuntime::on_disk().await;
        let sessions = root.path().join("sessions");
        let job = JobId::new(1).unwrap();
        store
            .append(agent.clone(), job_created(job, JobRole::Agent))
            .await
            .unwrap();
        let (field, body) = (
            FieldPointer::result().property("body"),
            vec![b'x'; 600 * 1024],
        );
        let stage = |store: &SessionStore| {
            let kind = (CaptureKind::Text, None);
            store
                .outputs()
                .stage_capture(run(job, 0), &field, kind, StagedBytes::Rows(&body))
        };
        stage(&store).unwrap();
        assert!(
            store
                .test_reads()
                .saved(run(job, 0))
                .unwrap()
                .captures
                .is_empty()
        );
        store.close().await.unwrap();
        let reopened = reopen(&sessions, store.id()).await;
        let count = |table: &str| {
            let sql = format!("SELECT count(*) FROM {table}");
            let read = |db: &Db| db.query_row(sql, Vec::new(), |row| Ok(row.get::<i64>(0)?));
            reopened.test_reads().read(read).unwrap()
        };
        for table in ["job_capture", "job_capture_chunk", "job_capture_staged"] {
            assert_eq!(count(table), Some(0), "{table}");
        }
        let rows = OutputRows {
            run: run(job, 0),
            result: Some(r#"{"body":""}"#.into()),
            captures_complete: true,
            fields: Vec::new(),
            staged: vec![stage(&reopened).unwrap()],
            files: Vec::new(),
            presented: Presented::new(),
        };
        let finished = SessionEvent::JobFinished {
            job,
            state: JobEnd::Completed,
            diagnostic: None,
            images: Vec::new(),
        };
        let entries = vec![(agent.clone(), finished)];
        let plan = |_: &CoreState| Ok::<_, SessionError>(Plan::journal(entries).saving([Ok(rows)]));
        reopened.commit(Command::Journal, plan).await.unwrap();
        let saved = reopened
            .test_reads()
            .saved(run(job, 0))
            .unwrap()
            .output
            .unwrap();
        assert_eq!(saved.fields, std::slice::from_ref(&field));
        assert_eq!(count("job_capture_staged"), Some(0));
        reopened.close().await.unwrap();
    }

    /// Closing revokes every clone's writes, including SQL queries that insert rows.
    #[tokio::test]
    async fn closed_output_handles_cannot_write_to_a_reopened_session() {
        let TestRuntime {
            root, store, agent, ..
        } = TestRuntime::on_disk().await;
        let sessions = root.path().join("sessions");
        let job = JobId::new(1).unwrap();
        store
            .append(agent.clone(), job_created(job, JobRole::Agent))
            .await
            .unwrap();
        let (old, retained) = (store.outputs(), store.test_reads());
        let field = FieldPointer::result().property("text");
        let reserve = |db: &SharedDb, generation| {
            let capture = db.create_capture(run(job, generation), &field, CaptureKind::Text);
            capture.unwrap().unwrap()
        };
        let previous = reserve(&old, 0);
        let extent = old
            .append_capture(previous, CaptureExtent::default(), b"previous")
            .unwrap();
        store
            .append(
                agent.clone(),
                SessionEvent::JobFinished {
                    job,
                    state: JobEnd::Interrupted,
                    diagnostic: None,
                    images: Vec::new(),
                },
            )
            .await
            .unwrap();
        store.close().await.unwrap();
        assert!(old.append_capture(previous, extent, b"late").is_err());

        let reopened = reopen(&sessions, store.id()).await;
        let restart = SessionEvent::JobStateChanged {
            job,
            state: JobTransition::Running,
        };
        let stale = store.append(agent.clone(), restart.clone()).await;
        assert_matches!(
            stale,
            Err(SessionError::Refused(crate::session::SessionPhase::Closed))
        );
        reopened.append(agent.clone(), restart).await.unwrap();
        let current = reopened.outputs();
        let capture = reserve(&current, 1);
        let run = run(job, 1);
        current
            .append_capture(capture, CaptureExtent::default(), b"current")
            .unwrap();
        finish(&reopened, &agent, (job, 1), "{}", &[capture])
            .await
            .unwrap();

        let extra = FieldPointer::result().property("late");
        let reserved = old.create_capture(run, &extra, CaptureKind::Text);
        assert!(reserved.is_err(), "a closed handle inserted a capture");
        // A retained reader sees the new owner's committed output.
        let saved = retained.saved(run).unwrap().output.unwrap();
        assert_eq!(saved.result.as_deref(), Some("{}"));
        assert_eq!(saved.fields.as_slice(), std::slice::from_ref(&field));
        reopened.close().await.unwrap();
    }
}

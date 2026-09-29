//! Job output rows per generation. Capture chunks record the newlines before them,
//! so line seeks are indexed lookups.

use super::{
    Db, DbError, DbResult, SharedDb, corrupt, enum_column, optional_enum_column, params, rejected,
    u64_of as integer,
};
use crate::{
    job::{CaptureKind, output::Detection},
    tool::output::FieldPointer,
};

/// Largest chunk row written; readers accept any length the schema allows.
const CHUNK_BYTES: usize = 256 * 1024;
/// Chunk rows one read fetches at most; short writes leave rows below `CHUNK_BYTES`.
const CHUNK_WINDOW: usize = 16;

pub(crate) struct CaptureRow {
    pub id: i64,
    pub pointer: FieldPointer,
    pub kind: CaptureKind,
    /// The saved terminal document references this capture as a result field.
    pub referenced: bool,
    /// Set once a finished capture its schema declares as JSON is classified.
    pub detection: Option<Detection>,
}

/// Committed capture length, newline count, and whether the bytes end a line.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CaptureExtent {
    pub bytes: u64,
    pub newlines: u64,
    pub ends_line: bool,
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

fn generation(db: &Db, job: u64) -> DbResult<i64> {
    db.query_row(
        "SELECT generation FROM job_generation WHERE job = ?1",
        params![job],
        |row| Ok(row.get::<i64>(0)?),
    )?
    .ok_or_else(|| rejected(format!("job {job} has no journaled creation")))
}

fn extent(db: &Db, capture: i64) -> DbResult<CaptureExtent> {
    Ok(db
        .query_row(
            "SELECT byte_offset, first_line, data FROM job_capture_chunk \
             WHERE capture = ?1 ORDER BY byte_offset DESC LIMIT 1",
            params![capture],
            |row| {
                let data = row.get::<Vec<u8>>(2)?;
                Ok(CaptureExtent {
                    bytes: integer(row.get::<i64>(0)?) + data.len() as u64,
                    newlines: integer(row.get::<i64>(1)?)
                        + data.iter().filter(|&&byte| byte == b'\n').count() as u64,
                    ends_line: data.last() == Some(&b'\n'),
                })
            },
        )?
        .unwrap_or_default())
}

impl SharedDb {
    /// Reserve `pointer` in the job's current generation; `None` if already reserved.
    pub(crate) fn create_capture(
        &self,
        job: u64,
        pointer: &FieldPointer,
        kind: CaptureKind,
        rendered: bool,
    ) -> Result<Option<i64>, DbError> {
        let db = self.lock();
        let generation = generation(&db, job)?;
        db.query_row(
            "INSERT INTO job_capture (job, generation, pointer, capture_kind, rendered) \
             VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (job, generation, pointer, rendered) DO NOTHING \
             RETURNING id",
            params![job, generation, pointer, kind, rendered],
            |row| Ok(row.get::<i64>(0)?),
        )
    }

    /// A finished cached rendering of `pointer` in the current generation.
    pub(crate) fn rendering(
        &self,
        job: u64,
        pointer: &FieldPointer,
    ) -> Result<Option<i64>, DbError> {
        self.lock().query_row(
            "SELECT c.id FROM job_capture c JOIN job_generation g \
               ON g.job = c.job AND g.generation = c.generation \
             WHERE c.job = ?1 AND c.pointer = ?2 AND c.rendered = 1 \
               AND c.final_bytes IS NOT NULL",
            params![job, pointer],
            |row| Ok(row.get::<i64>(0)?),
        )
    }

    pub(crate) fn delete_capture(&self, capture: i64) -> Result<(), DbError> {
        self.lock()
            .execute("DELETE FROM job_capture WHERE id = ?1", params![capture])
            .map(drop)
    }

    pub(crate) fn resolve_capture_kind(
        &self,
        capture: i64,
        kind: CaptureKind,
    ) -> Result<(), DbError> {
        self.lock()
            .execute(
                "UPDATE job_capture SET capture_kind = ?2 WHERE id = ?1 AND capture_kind <> ?2",
                params![capture, kind],
            )
            .map(drop)
    }

    pub(crate) fn set_detection(&self, capture: i64, detection: Detection) -> Result<(), DbError> {
        self.lock()
            .execute(
                "UPDATE job_capture SET detection = ?2 WHERE id = ?1",
                params![capture, detection],
            )
            .map(drop)
    }

    pub(crate) fn finish_capture(&self, capture: i64) -> Result<(), DbError> {
        let db = self.lock();
        let extent = extent(&db, capture)?;
        let lines = extent.newlines + u64::from(extent.bytes > 0 && !extent.ends_line);
        db.execute(
            "UPDATE job_capture SET final_bytes = ?2, final_lines = ?3 WHERE id = ?1",
            params![capture, extent.bytes, lines],
        )
        .map(drop)
    }

    /// Captures of the job's current generation, ordered by pointer.
    pub(crate) fn captures(&self, job: u64) -> Result<Vec<CaptureRow>, DbError> {
        self.lock().query(
            "SELECT c.id, c.pointer, c.capture_kind, \
               EXISTS (SELECT 1 FROM job_output_field f WHERE f.capture = c.id), c.detection \
             FROM job_capture c JOIN job_generation g \
               ON g.job = c.job AND g.generation = c.generation \
             WHERE c.job = ?1 AND c.rendered = 0 ORDER BY c.pointer",
            params![job],
            |row| {
                Ok(CaptureRow {
                    id: row.get(0)?,
                    pointer: pointer(row, 1)?,
                    kind: enum_column(row, 2)?,
                    referenced: row.get::<i64>(3)? != 0,
                    detection: optional_enum_column(row, 4)?,
                })
            },
        )
    }

    /// Append `data` at `extent`, returning the new extent. Captures are not ledger
    /// state: they commit without fsync, and the next ledger commit makes them durable.
    pub(crate) fn append_capture(
        &self,
        capture: i64,
        mut extent: CaptureExtent,
        data: &[u8],
    ) -> Result<CaptureExtent, DbError> {
        if data.is_empty() {
            return Ok(extent);
        }
        let db = self.lock();
        db.batch("PRAGMA synchronous = NORMAL")?;
        let appended = db.atomic(|| {
            for piece in data.chunks(CHUNK_BYTES) {
                db.execute(
                    "INSERT INTO job_capture_chunk (capture, byte_offset, first_line, data) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![capture, extent.bytes, extent.newlines, piece],
                )?;
                extent.bytes += piece.len() as u64;
                extent.newlines += piece.iter().filter(|&&byte| byte == b'\n').count() as u64;
                extent.ends_line = piece.last() == Some(&b'\n');
            }
            Ok(())
        });
        // Restore ledger durability whatever the append's outcome; the append stands.
        let restored = db.batch("PRAGMA synchronous = FULL");
        appended?;
        restored.map(|()| extent)
    }

    /// Discard bytes from `length` on, returning the remaining extent.
    pub(crate) fn truncate_capture(
        &self,
        capture: i64,
        length: u64,
    ) -> Result<CaptureExtent, DbError> {
        let db = self.lock();
        db.atomic(|| {
            db.execute(
                "DELETE FROM job_capture_chunk WHERE capture = ?1 AND byte_offset >= ?2",
                params![capture, length],
            )?;
            db.execute(
                "UPDATE job_capture_chunk SET data = substr(data, 1, ?2 - byte_offset) \
                 WHERE capture = ?1 AND byte_offset < ?2 AND byte_offset + length(data) > ?2",
                params![capture, length],
            )?;
            extent(&db, capture)
        })
    }

    pub(crate) fn capture_extent(&self, capture: i64) -> Result<CaptureExtent, DbError> {
        extent(&self.lock(), capture)
    }

    /// The last chunk starting before one-based `line` begins: its offset and line.
    pub(crate) fn capture_line(&self, capture: i64, line: u64) -> Result<(u64, u64), DbError> {
        if line <= 1 {
            return Ok((0, 1));
        }
        Ok(self
            .lock()
            .query_row(
                "SELECT byte_offset, first_line FROM job_capture_chunk \
                 WHERE capture = ?1 AND first_line <= ?2 \
                 ORDER BY first_line DESC, byte_offset DESC LIMIT 1",
                params![capture, line - 2],
                |row| Ok((integer(row.get(0)?), integer(row.get::<i64>(1)?) + 1)),
            )?
            .unwrap_or((0, 1)))
    }

    /// Consecutive chunks from the one containing `offset`, up to about `maximum` bytes.
    pub(crate) fn capture_chunks(
        &self,
        capture: i64,
        offset: u64,
        maximum: usize,
    ) -> Result<Vec<(u64, Vec<u8>)>, DbError> {
        let rows = self.lock().query(
            "SELECT byte_offset, data FROM job_capture_chunk WHERE capture = ?1 \
               AND byte_offset >= (SELECT coalesce(max(byte_offset), 0) FROM job_capture_chunk \
                 WHERE capture = ?1 AND byte_offset <= ?2) \
             ORDER BY byte_offset LIMIT ?3",
            params![capture, offset, CHUNK_WINDOW],
            |row| Ok((integer(row.get(0)?), row.get::<Vec<u8>>(1)?)),
        )?;
        let mut total = 0;
        Ok(rows
            .into_iter()
            .take_while(|chunk| {
                let take = total < maximum;
                total += chunk.1.len();
                take
            })
            .collect())
    }

    /// Save the current generation's product and its referenced captures, once per
    /// run: the fields its schema declares complete join those a script recorded
    /// before it finished, and its presentation is measured afresh.
    pub(crate) fn save_output(
        &self,
        job: u64,
        result: Option<&str>,
        captures_complete: bool,
        referenced: &[i64],
        complete: &[FieldPointer],
    ) -> Result<(), DbError> {
        let db = self.lock();
        db.atomic(|| {
            let generation = generation(&db, job)?;
            let output = db
                .query_row(
                    "INSERT INTO job_output (job, generation, captures_complete, result) \
                     VALUES (?1, ?2, ?3, ?4) ON CONFLICT (job, generation) DO UPDATE \
                     SET captures_complete = excluded.captures_complete, \
                     result = excluded.result, presented_bytes = NULL RETURNING id",
                    params![job, generation, captures_complete, result],
                    |row| Ok(row.get::<i64>(0)?),
                )?
                .ok_or_else(|| rejected("job output upsert returned no row"))?;
            db.execute(
                "DELETE FROM job_output_field WHERE output = ?1",
                params![output],
            )?;
            // Renderings of the replaced document are stale.
            db.execute(
                "DELETE FROM job_capture WHERE job = ?1 AND generation = ?2 AND rendered = 1",
                params![job, generation],
            )?;
            for capture in referenced {
                db.execute(
                    "INSERT INTO job_output_field (output, capture, job, generation) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![output, *capture, job, generation],
                )?;
            }
            for pointer in complete {
                db.execute(
                    "INSERT INTO job_complete_field (job, generation, pointer) \
                     VALUES (?1, ?2, ?3) ON CONFLICT DO NOTHING",
                    params![job, generation, pointer],
                )?;
            }
            Ok(())
        })
    }

    pub(crate) fn set_presented_bytes(&self, job: u64, bytes: usize) -> Result<(), DbError> {
        let db = self.lock();
        let generation = generation(&db, job)?;
        let bytes = i64::try_from(bytes).unwrap_or(i64::MAX);
        db.execute(
            "UPDATE job_output SET presented_bytes = ?3 \
             WHERE job = ?1 AND generation = ?2 AND presented_bytes IS NULL",
            params![job, generation, bytes],
        )?;
        Ok(())
    }

    /// The current run's measured presentation size, once measured.
    pub(crate) fn presented_bytes(&self, job: u64) -> Result<Option<usize>, DbError> {
        let db = self.lock();
        let generation = generation(&db, job)?;
        let bytes = db
            .query_row(
                "SELECT presented_bytes FROM job_output WHERE job = ?1 AND generation = ?2",
                params![job, generation],
                |row| Ok(row.get::<Option<i64>>(0)?),
            )?
            .flatten();
        Ok(bytes.map(|bytes| usize::try_from(bytes).unwrap_or(usize::MAX)))
    }

    /// The current generation's product, once the run finished.
    pub(crate) fn output(&self, job: u64) -> Result<Option<SavedOutput>, DbError> {
        let db = self.lock();
        let Some((output, result, captures_complete)) = db.query_row(
            "SELECT o.id, o.result, o.captures_complete FROM job_output o \
             JOIN job_generation g ON g.job = o.job AND g.generation = o.generation \
             WHERE o.job = ?1",
            params![job],
            |row| {
                Ok((
                    row.get::<i64>(0)?,
                    row.get::<Option<String>>(1)?,
                    row.get::<bool>(2)?,
                ))
            },
        )?
        else {
            return Ok(None);
        };
        let fields = db.query(
            "SELECT c.pointer FROM job_output_field f JOIN job_capture c ON c.id = f.capture \
             WHERE f.output = ?1 ORDER BY c.pointer",
            params![output],
            |row| pointer(row, 0),
        )?;
        Ok(Some(SavedOutput {
            result,
            captures_complete,
            fields,
        }))
    }

    pub(crate) fn save_complete_fields(
        &self,
        job: u64,
        fields: &[FieldPointer],
    ) -> Result<(), DbError> {
        let db = self.lock();
        db.atomic(|| {
            let generation = generation(&db, job)?;
            db.execute(
                "DELETE FROM job_complete_field WHERE job = ?1 AND generation = ?2",
                params![job, generation],
            )?;
            for pointer in fields {
                db.execute(
                    "INSERT INTO job_complete_field (job, generation, pointer) \
                     VALUES (?1, ?2, ?3)",
                    params![job, generation, pointer],
                )?;
            }
            Ok(())
        })
    }

    pub(crate) fn complete_fields(&self, job: u64) -> Result<Vec<FieldPointer>, DbError> {
        self.lock().query(
            "SELECT p.pointer FROM job_complete_field p JOIN job_generation g \
               ON g.job = p.job AND g.generation = p.generation \
             WHERE p.job = ?1 ORDER BY p.id",
            params![job],
            |row| pointer(row, 0),
        )
    }

    #[cfg(test)]
    pub(crate) fn test_batch(&self, sql: &str) {
        self.lock().batch(sql).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        execution::ExecutionLocation,
        identity::JobId,
        job::{JobEnd, JobRole, JobTransition},
        session::{SessionError, SessionEvent, SessionStore, tests::on_disk},
    };

    /// Closing revokes every clone's writes, including SQL queries that insert rows
    /// and mutations that resolve the job's current generation only when they run.
    #[tokio::test]
    async fn closed_output_handles_cannot_write_to_a_reopened_session() {
        let (root, store, agent) = on_disk().await;
        let job = JobId::new(1).unwrap();
        store
            .append(
                agent.clone(),
                SessionEvent::JobCreated {
                    job,
                    parent: None,
                    origin: None,
                    tool: "agent".into(),
                    role: JobRole::Agent,
                    name: None,
                    arguments: serde_json::json!({}),
                    output_schema: None,
                    accepts_input: true,
                    background: false,
                    location: ExecutionLocation::root(root.path().to_owned()),
                },
            )
            .await
            .unwrap();
        let old = store.outputs();
        let field = FieldPointer::result().property("text");
        let reserve = |db: &SharedDb, kind| {
            db.create_capture(job.get(), &field, kind, false)
                .unwrap()
                .unwrap()
        };
        let previous = reserve(&old, CaptureKind::Unknown);
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
                    output_diagnostic: None,
                    images: Vec::new(),
                },
            )
            .await
            .unwrap();
        store.close().await.unwrap();
        assert!(old.append_capture(previous, extent, b"late").is_err());

        let (reopened, _) = SessionStore::open(root.path(), store.id()).await.unwrap();
        let restart = SessionEvent::JobStateChanged {
            job,
            state: JobTransition::Running,
        };
        let stale = store.append(agent.clone(), restart.clone()).await;
        assert!(matches!(stale, Err(SessionError::Closed)));
        reopened.append(agent, restart).await.unwrap();
        let current = reopened.outputs();
        let capture = reserve(&current, CaptureKind::Text);
        current
            .append_capture(capture, CaptureExtent::default(), b"current")
            .unwrap();
        current.finish_capture(capture).unwrap();
        let document = r#"{"text":""}"#;
        current
            .save_output(job.get(), Some(document), false, &[capture], &[])
            .unwrap();

        let extra = FieldPointer::result().property("late");
        for (operation, result) in [
            (
                "reserve",
                old.create_capture(job.get(), &extra, CaptureKind::Text, false)
                    .map(drop),
            ),
            (
                "render",
                old.create_capture(job.get(), &field, CaptureKind::Text, true)
                    .map(drop),
            ),
            (
                "append",
                old.append_capture(previous, extent, b"late").map(drop),
            ),
            ("truncate", old.truncate_capture(previous, 0).map(drop)),
            ("delete", old.delete_capture(previous)),
            (
                "kind",
                old.resolve_capture_kind(previous, CaptureKind::Text),
            ),
            ("detection", old.set_detection(capture, Detection::NotJson)),
            ("finish", old.finish_capture(previous)),
            (
                "document",
                old.save_output(job.get(), Some("null"), true, &[], &[]),
            ),
            ("complete", old.save_complete_fields(job.get(), &[extra])),
            ("size", old.set_presented_bytes(job.get(), 42)),
        ] {
            assert!(result.is_err(), "closed handle wrote {operation}");
        }

        // Retained readers see the new owner's committed data, without changing it.
        let saved = old.output(job.get()).unwrap().unwrap();
        assert_eq!(saved.result.as_deref(), Some(document));
        assert!(!saved.captures_complete);
        assert_eq!(saved.fields.as_slice(), std::slice::from_ref(&field));
        assert!(old.complete_fields(job.get()).unwrap().is_empty());
        assert_eq!(old.presented_bytes(job.get()).unwrap(), None);
        assert_eq!(old.rendering(job.get(), &field).unwrap(), None);
        let captures = old.captures(job.get()).unwrap();
        assert_eq!(captures.len(), 1);
        assert_eq!(captures[0].id, capture);
        assert_eq!(captures[0].kind, CaptureKind::Text);
        assert_eq!(captures[0].detection, None);
        for (capture, bytes) in [
            (previous, b"previous".as_slice()),
            (capture, b"current".as_slice()),
        ] {
            let chunks = old.capture_chunks(capture, 0, usize::MAX).unwrap();
            assert_eq!(chunks, [(0, bytes.to_vec())]);
        }
        current
            .resolve_capture_kind(previous, CaptureKind::Text)
            .unwrap();
        current.finish_capture(previous).unwrap();
        current.set_detection(capture, Detection::NotJson).unwrap();
        reopened.close().await.unwrap();
    }
}

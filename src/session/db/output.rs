//! Job output rows per generation. Capture chunks record the newlines before them,
//! so line seeks are indexed lookups.

use super::{
    Db, DbError, DbResult, SharedDb, corrupt, enum_column, optional_enum_column, params,
    u64_of as integer,
};
use crate::{
    job::{
        CaptureKind,
        output::{Detection, Presented},
    },
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

impl CaptureExtent {
    /// Lines the bytes hold, counting an unterminated last line.
    fn lines(self) -> u64 {
        self.newlines + u64::from(self.bytes > 0 && !self.ends_line)
    }

    /// The extent once `data` follows.
    fn after(self, data: &[u8]) -> Self {
        let newlines = data.iter().filter(|&&byte| byte == b'\n').count() as u64;
        Self {
            bytes: self.bytes + data.len() as u64,
            newlines: self.newlines + newlines,
            ends_line: data.last().map_or(self.ends_line, |&byte| byte == b'\n'),
        }
    }
}

/// Insert `data` as chunk rows of `capture` from `extent`, returning the extent after.
fn write_chunks(
    db: &Db,
    capture: i64,
    mut extent: CaptureExtent,
    data: &[u8],
) -> DbResult<CaptureExtent> {
    for piece in data.chunks(CHUNK_BYTES) {
        db.execute(
            "INSERT INTO job_capture_chunk (capture, byte_offset, first_line, data) \
             VALUES (?1, ?2, ?3, ?4)",
            params![capture, extent.bytes, extent.newlines, piece],
        )?;
        extent = extent.after(piece);
    }
    Ok(extent)
}

/// One run's product, prepared for the transaction that journals the run's
/// finish, which saves it once: a refused finish leaves none of it.
pub(crate) struct OutputRows {
    pub run: crate::job::Run,
    pub result: Option<String>,
    pub captures_complete: bool,
    /// Kinds the run's admitted captures resolve to.
    pub kinds: Vec<(i64, CaptureKind)>,
    /// Registered captures the result references, with what candidate text holds.
    pub fields: Vec<(i64, Option<Detection>)>,
    /// Parts of the result stored as captures of their own.
    pub offloaded: Vec<OffloadedCapture>,
    pub presented: Presented,
}

/// A finished capture of bytes offloaded from a result.
pub(crate) struct OffloadedCapture {
    pub pointer: FieldPointer,
    pub kind: CaptureKind,
    pub detection: Option<Detection>,
    pub data: Vec<u8>,
}

impl OutputRows {
    pub(in crate::session) fn insert(self, db: &Db) -> DbResult<()> {
        let Self {
            run,
            result,
            captures_complete,
            kinds,
            fields,
            offloaded,
            presented,
        } = self;
        let (job, generation) = run.key();
        db.execute(
            "INSERT INTO job_output (job, generation, captures_complete, result) \
             VALUES (?1, ?2, ?3, ?4)",
            params![job, generation, captures_complete, result],
        )?;
        for (capture, kind) in kinds {
            db.execute(
                "UPDATE job_capture SET capture_kind = ?2 WHERE id = ?1 AND capture_kind <> ?2",
                params![capture, kind],
            )?;
        }
        let mut referenced = Vec::with_capacity(fields.len() + offloaded.len());
        for (capture, detection) in fields {
            if let Some(detection) = detection {
                db.execute(
                    "UPDATE job_capture SET detection = ?2 WHERE id = ?1",
                    params![capture, detection],
                )?;
            }
            referenced.push(capture);
        }
        for offloaded in offloaded {
            let extent = CaptureExtent::default().after(&offloaded.data);
            let capture = db.insert(
                "INSERT INTO job_capture \
                   (job, generation, pointer, capture_kind, final_bytes, final_lines, detection) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    job,
                    generation,
                    &offloaded.pointer,
                    offloaded.kind,
                    extent.bytes,
                    extent.lines(),
                    offloaded.detection
                ],
            )?;
            write_chunks(db, capture, CaptureExtent::default(), &offloaded.data)?;
            referenced.push(capture);
        }
        for capture in referenced {
            db.execute(
                "INSERT INTO job_output_field (job, generation, capture) VALUES (?1, ?2, ?3)",
                params![job, generation, capture],
            )?;
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
        self.lock().query_row(
            "INSERT INTO job_capture (job, generation, pointer, capture_kind) \
             VALUES (?1, ?2, ?3, ?4) ON CONFLICT (job, generation, pointer) DO NOTHING \
             RETURNING id",
            params![job, generation, pointer, kind],
            |row| Ok(row.get::<i64>(0)?),
        )
    }

    pub(crate) fn delete_capture(&self, capture: i64) -> Result<(), DbError> {
        self.lock()
            .execute("DELETE FROM job_capture WHERE id = ?1", params![capture])
            .map(drop)
    }

    pub(crate) fn finish_capture(&self, capture: i64) -> Result<(), DbError> {
        let db = self.lock();
        let extent = extent(&db, capture)?;
        db.execute(
            "UPDATE job_capture SET final_bytes = ?2, final_lines = ?3 WHERE id = ?1",
            params![capture, extent.bytes, extent.lines()],
        )
        .map(drop)
    }

    /// Captures of run `generation` of `job`, ordered by pointer.
    pub(crate) fn captures(&self, run: crate::job::Run) -> Result<Vec<CaptureRow>, DbError> {
        let (job, generation) = run.key();
        self.lock().query(
            "SELECT c.id, c.pointer, c.capture_kind, \
               EXISTS (SELECT 1 FROM job_output_field f WHERE f.capture = c.id), c.detection \
             FROM job_capture c WHERE c.job = ?1 AND c.generation = ?2 ORDER BY c.pointer",
            params![job, generation],
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
        extent: CaptureExtent,
        data: &[u8],
    ) -> Result<CaptureExtent, DbError> {
        if data.is_empty() {
            return Ok(extent);
        }
        let db = self.lock();
        db.batch("PRAGMA synchronous = NORMAL")?;
        let appended = db.atomic(|| write_chunks(&db, capture, extent, data));
        // Restore ledger durability whatever the append's outcome; the append stands.
        let restored = db.batch("PRAGMA synchronous = FULL");
        let extent = appended?;
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

    /// The run's product, once it finished.
    pub(crate) fn output(&self, run: crate::job::Run) -> Result<Option<SavedOutput>, DbError> {
        let (job, generation) = run.key();
        let db = self.lock();
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

    pub(crate) fn presented_fields(&self, run: crate::job::Run) -> Result<Presented, DbError> {
        let (job, generation) = run.key();
        let fields = self.lock().query(
            "SELECT pointer, presentation FROM job_field_presentation \
             WHERE job = ?1 AND generation = ?2",
            params![job, generation],
            |row| Ok((pointer(row, 0)?, enum_column(row, 1)?)),
        )?;
        Ok(fields.into_iter().collect())
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
        identity::{AgentId, JobId},
        job::{JobEnd, JobRole, JobTransition},
        session::{
            Command, CoreState, Plan, SessionError, SessionEvent, SessionStore,
            tests::{job_created, on_disk, reopen},
        },
    };

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
            kinds: Vec::new(),
            fields: captures.iter().map(|&capture| (capture, None)).collect(),
            offloaded: Vec::new(),
            presented: Presented::new(),
        };
        let finished = SessionEvent::JobFinished {
            job,
            state: JobEnd::Completed,
            diagnostic: None,
            output_diagnostic: None,
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
        let (root, store, agent) = on_disk().await;
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
            let output = store.outputs().output(run(first, generation)).unwrap();
            output.map(|output| {
                assert_eq!(output.result.as_deref(), Some("{}"));
                output.fields
            })
        };
        let refused = async |store: SessionStore, generation, capture| {
            let records = store.records();
            let finish = finish(&store, &agent, (first, generation), "{}", &[capture]).await;
            assert!(matches!(finish, Err(SessionError::JournalFailed(_))));
            assert_eq!(store.records(), records);
            assert_eq!(saved(&store, generation), None);
            assert!(store.close().await.is_err());
            reopen(root.path(), store.id()).await.0
        };
        let earlier = reserve(&store, first, 0, "earlier");
        let foreign = reserve(&store, other, 0, "foreign");
        let store = refused(store, 0, foreign).await;
        finish(&store, &agent, (first, 0), "{}", &[earlier])
            .await
            .unwrap();
        assert_eq!(saved(&store, 0), Some(vec![field("earlier")]));
        for change in [
            "UPDATE job_output SET result = 'null'",
            "DELETE FROM job_output_field",
        ] {
            assert!(store.outputs().lock().execute(change, Vec::new()).is_err());
        }
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

    /// Closing revokes every clone's writes, including SQL queries that insert rows.
    #[tokio::test]
    async fn closed_output_handles_cannot_write_to_a_reopened_session() {
        let (root, store, agent) = on_disk().await;
        let job = JobId::new(1).unwrap();
        store
            .append(agent.clone(), job_created(job, JobRole::Agent))
            .await
            .unwrap();
        let old = store.outputs();
        let field = FieldPointer::result().property("text");
        let reserve = |db: &SharedDb, generation, kind| {
            let capture = db.create_capture(run(job, generation), &field, kind);
            capture.unwrap().unwrap()
        };
        let previous = reserve(&old, 0, CaptureKind::Unknown);
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

        let (reopened, _) = reopen(root.path(), store.id()).await;
        let restart = SessionEvent::JobStateChanged {
            job,
            state: JobTransition::Running,
        };
        let stale = store.append(agent.clone(), restart.clone()).await;
        assert!(matches!(
            stale,
            Err(SessionError::Refused(crate::session::SessionPhase::Closed))
        ));
        reopened.append(agent.clone(), restart).await.unwrap();
        let current = reopened.outputs();
        let capture = reserve(&current, 1, CaptureKind::Text);
        let run = run(job, 1);
        current
            .append_capture(capture, CaptureExtent::default(), b"current")
            .unwrap();
        current.finish_capture(capture).unwrap();
        finish(&reopened, &agent, (job, 1), "{}", &[capture])
            .await
            .unwrap();

        let extra = FieldPointer::result().property("late");
        for (operation, result) in [
            (
                "reserve",
                old.create_capture(run, &extra, CaptureKind::Text).map(drop),
            ),
            (
                "append",
                old.append_capture(previous, extent, b"late").map(drop),
            ),
            ("truncate", old.truncate_capture(previous, 0).map(drop)),
            ("delete", old.delete_capture(previous)),
            ("finish", old.finish_capture(previous)),
        ] {
            assert!(result.is_err(), "closed handle wrote {operation}");
        }

        // Retained readers see the new owner's committed data, without changing it.
        let saved = old.output(run).unwrap().unwrap();
        assert_eq!(saved.result.as_deref(), Some("{}"));
        assert_eq!(saved.fields.as_slice(), std::slice::from_ref(&field));
        let captures = old.captures(run).unwrap();
        assert_eq!(captures.len(), 1);
        assert_eq!(captures[0].id, capture);
        assert_eq!(captures[0].kind, CaptureKind::Text);
        for (capture, bytes) in [
            (previous, b"previous".as_slice()),
            (capture, b"current".as_slice()),
        ] {
            let chunks = old.capture_chunks(capture, 0, usize::MAX).unwrap();
            assert_eq!(chunks, [(0, bytes.to_vec())]);
        }
        current.finish_capture(previous).unwrap();
        reopened.close().await.unwrap();
    }
}

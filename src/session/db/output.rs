//! Job output rows per generation. Capture chunks record the newlines before them,
//! so line seeks are indexed lookups.

use super::{
    Db, DbError, DbResult, Reads, SharedDb, SqlText, corrupt, enum_column, optional_enum_column,
    params, u64_of as integer,
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
/// Bytes of a staged capture written between syncs of the log.
const STAGED_SYNC_BYTES: usize = 4 * CHUNK_BYTES;

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
    pub presented: Presented,
}

/// A finished capture of bytes offloaded from a result, written ahead of the finish
/// that references it, so the finish's transaction stays small. Until that commits,
/// the capture is staged: no reader lists it, and opening the session removes it.
pub(crate) struct StagedCapture(i64);

impl OutputRows {
    pub(in crate::session) fn insert(self, db: &Db) -> DbResult<()> {
        let Self {
            run,
            result,
            captures_complete,
            fields,
            staged,
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
    /// reach the disk.
    pub(crate) fn stage_capture(
        &self,
        run: crate::job::Run,
        pointer: &FieldPointer,
        (kind, detection): (CaptureKind, Option<Detection>),
        data: &[u8],
    ) -> Result<StagedCapture, DbError> {
        let (job, generation) = run.key();
        let capture = self.write(|db| {
            let capture = db.insert(
                "INSERT INTO job_capture (job, generation, pointer, capture_kind, detection) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![job, generation, pointer, kind, detection],
            )?;
            db.execute(
                "INSERT INTO job_capture_staged (capture) VALUES (?1)",
                params![capture],
            )?;
            Ok(capture)
        })?;
        let mut extent = CaptureExtent::default();
        for synced in data.chunks(STAGED_SYNC_BYTES) {
            for piece in synced.chunks(CHUNK_BYTES) {
                extent = self.append_capture(capture, extent, piece)?;
            }
            self.sync()?;
        }
        Ok(StagedCapture(capture))
    }

    pub(crate) fn delete_capture(&self, capture: i64) -> Result<(), DbError> {
        let delete =
            |db: &Db| db.execute("DELETE FROM job_capture WHERE id = ?1", params![capture]);
        Ok(self.write(delete).map(drop)?)
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

    /// Discard bytes from `length` on, returning the remaining extent.
    pub(crate) fn truncate_capture(
        &self,
        capture: i64,
        length: u64,
    ) -> Result<CaptureExtent, DbError> {
        let truncated = self.write(|db| {
            db.execute(
                "DELETE FROM job_capture_chunk WHERE capture = ?1 AND byte_offset >= ?2",
                params![capture, length],
            )?;
            db.execute(
                "UPDATE job_capture_chunk SET data = substr(data, 1, ?2 - byte_offset) \
                 WHERE capture = ?1 AND byte_offset < ?2 AND byte_offset + length(data) > ?2",
                params![capture, length],
            )?;
            extent(db, capture)
        });
        Ok(truncated?)
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

fn captures(db: &Db, (job, generation): (u64, i64)) -> DbResult<Vec<CaptureRow>> {
    db.query(
        "SELECT c.id, c.pointer, c.capture_kind, \
           EXISTS (SELECT 1 FROM job_output_field f WHERE f.capture = c.id), c.detection \
         FROM job_capture c WHERE c.job = ?1 AND c.generation = ?2 \
           AND NOT EXISTS (SELECT 1 FROM job_capture_staged s WHERE s.capture = c.id) \
         ORDER BY c.pointer",
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

fn saved_output(db: &Db, (job, generation): (u64, i64)) -> DbResult<Option<SavedOutput>> {
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

    pub(crate) fn capture_extent(&self, capture: i64) -> Result<CaptureExtent, DbError> {
        self.read(|db| extent(db, capture))
    }

    /// The last chunk starting before one-based `line` begins: its offset and line.
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

    /// The consecutive chunks overlapping the `length` bytes from `offset`.
    pub(crate) fn capture_chunks(
        &self,
        capture: i64,
        offset: u64,
        length: usize,
    ) -> Result<Vec<Chunk>, DbError> {
        self.read(|db| chunks(db, capture, offset, length))
    }

    /// The capture's extent with the chunks overlapping the `length` bytes from
    /// `offset`, read together.
    pub(crate) fn capture_window(
        &self,
        capture: i64,
        offset: u64,
        length: usize,
    ) -> Result<(CaptureExtent, Vec<Chunk>), DbError> {
        self.read(|db| Ok((extent(db, capture)?, chunks(db, capture, offset, length)?)))
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
            tests::{job_created, reopen},
        },
        tests::TestRuntime,
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
            fields: captures.iter().map(|&capture| (capture, None)).collect(),
            staged: Vec::new(),
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
            assert!(matches!(finish, Err(SessionError::JournalFailed(_))));
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
                .stage_capture(run(job, 0), &field, kind, &body)
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
        assert!(matches!(
            stale,
            Err(SessionError::Refused(crate::session::SessionPhase::Closed))
        ));
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

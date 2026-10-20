//! Jobs: creation, transitions, finishes with their diagnostics and images,
//! deliveries to owners, and the status rows the job fold writes.

use super::{
    decode::*,
    diagnostic,
    encode::*,
    message::{image, image_row},
    *,
};
use std::{collections::HashMap, sync::Arc};

use serde_json::Value;

use crate::{
    execution::path_bytes,
    identity::{AgentId, JobId, SessionId},
    job::{AgentProgress, ChildStatus, Invocation, JobStatus, RunTiming},
    media::BlobDigest,
    session::{EntryKind, EventRecord, ModelCallOrigin, RecordSeq, SessionEvent},
};

/// A job's status row, with the records its facts start from: its creation, its
/// latest finish while its state is a finish's, and the commits of its answer and
/// queued replies; and the child its launch installed.
pub(crate) struct JobRow {
    pub(crate) id: JobId,
    pub(crate) created: Arc<EventRecord>,
    pub(crate) finish: Option<Arc<EventRecord>>,
    pub(crate) child: Option<AgentId>,
    pub(crate) status: JobStatus,
    pub(crate) messages: HashMap<MessageSeq, Arc<EventRecord>>,
}

/// Write each job's status as the job fold leaves it, in the transaction of the
/// records that changed it.
pub(in crate::session) fn write_status(db: &Db, jobs: &[(JobId, JobStatus)]) -> DbResult<()> {
    for (job, status) in jobs {
        let (first_run, ended) = match status.timing {
            RunTiming::Unstarted => (None, None),
            RunTiming::Since(since) => (Some(since), None),
            RunTiming::Took { since, until } => (Some(since), Some(until)),
        };
        let child = status.child.as_ref();
        let (answer, kind) = child.and_then(|child| child.answer).unzip();
        let progress = child.map(|child| (child.progress.turns, child.progress.tool_calls));
        let (turns, tool_calls) = progress.unzip();
        db.execute(
            "INSERT INTO job_status (job, state, invocation, background, cancel_recorded, \
               pending, first_run_millis, ended_millis, answer, answer_kind, turns, tool_calls, \
               resident) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) \
             ON CONFLICT (job) DO UPDATE SET state = ?2, invocation = ?3, background = ?4, \
               cancel_recorded = ?5, pending = ?6, first_run_millis = ?7, ended_millis = ?8, \
               answer = ?9, answer_kind = ?10, turns = ?11, tool_calls = ?12",
            params![
                *job,
                status.state,
                status.invocation.number(),
                status.background,
                status.cancel_recorded,
                status.pending,
                first_run,
                ended,
                answer,
                kind,
                turns,
                tool_calls,
                status.resident,
            ],
        )?;
        // Apart, as setting the partial index's column rewrites its entry even unchanged.
        db.execute(
            "UPDATE job_status SET resident = ?2 WHERE job = ?1 AND resident <> ?2",
            params![*job, status.resident],
        )?;
        let Some(child) = child else {
            continue;
        };
        db.execute("DELETE FROM job_reply WHERE job = ?1", params![*job])?;
        for &(message, stamp) in &child.replies {
            db.execute(
                "INSERT INTO job_reply (job, message, stamp) VALUES (?1, ?2, ?3)",
                params![*job, message, stamp],
            )?;
        }
    }
    Ok(())
}

/// The commits of a job's answer and queued replies.
fn messages(status: &JobStatus) -> impl Iterator<Item = MessageSeq> + '_ {
    let child = status.child.iter();
    let replies = child.clone().flat_map(|child| &child.replies);
    let answer = child.filter_map(|child| child.answer);
    (replies.map(|(message, _)| *message)).chain(answer.map(|(message, _)| message))
}

/// The status rows of those of `jobs` that exist, in id order.
pub(in crate::session) fn job_rows(
    db: &Db,
    session: SessionId,
    jobs: &[JobId],
) -> DbResult<Vec<JobRow>> {
    let ids = json(&jobs.iter().map(|job| job.get()).collect::<Vec<_>>())?;
    let mut replies: HashMap<i64, Vec<(MessageSeq, u64)>> = HashMap::new();
    let queued = db.query(
        "SELECT job, message, stamp FROM job_reply \
         WHERE job IN (SELECT value FROM json_each(?1)) ORDER BY job, stamp, message",
        params![ids.clone()],
        |row| {
            let reply = (sequence(row.get(1)?).message(), u64_of(row.get(2)?));
            Ok((row.get::<i64>(0)?, reply))
        },
    )?;
    for (job, reply) in queued {
        replies.entry(job).or_default().push(reply);
    }
    let rows = db.query(
        "SELECT s.job, j.created, (SELECT max(f.entry) FROM job_finish f WHERE f.job = s.job), \
           a.child_index, s.state, s.invocation, s.background, s.cancel_recorded, s.pending, \
           s.first_run_millis, s.ended_millis, s.answer, s.answer_kind, s.turns, s.tool_calls, \
           s.resident \
         FROM job_status s JOIN job j ON j.id = s.job LEFT JOIN agent a ON a.owner_job = s.job \
         WHERE s.job IN (SELECT value FROM json_each(?1)) ORDER BY s.job",
        params![ids],
        |row| {
            let state: crate::job::JobState = enum_column(row, 4)?;
            let finish = row.get::<Option<i64>>(2)?.map(sequence);
            let timing = match (row.get::<Option<i64>>(9)?, row.get::<Option<i64>>(10)?) {
                (None, _) => RunTiming::Unstarted,
                (Some(since), None) => RunTiming::Since(since),
                (Some(since), Some(until)) => RunTiming::Took { since, until },
            };
            let answer = row
                .get::<Option<i64>>(11)?
                .map(|entry| sequence(entry).message());
            let answer = answer.zip(optional_enum_column(row, 12)?);
            let replies = take(&mut replies, row.get(0)?);
            let child = match (row.get::<Option<i64>>(13)?, row.get::<Option<i64>>(14)?) {
                (Some(turns), Some(tool_calls)) => Some(ChildStatus {
                    progress: AgentProgress {
                        turns: u64_of(turns),
                        tool_calls: u64_of(tool_calls),
                    },
                    answer,
                    replies,
                }),
                _ => None,
            };
            let status = JobStatus {
                state,
                invocation: Invocation::nth(row.get(5)?),
                background: row.get(6)?,
                cancel_recorded: row.get(7)?,
                pending: row.get::<Option<i64>>(8)?.map(u64_of),
                timing,
                resident: row.get(15)?,
                child,
            };
            let finish = finish.filter(|_| state.is_terminal());
            let created = sequence(row.get(1)?);
            Ok((
                job(row.get(0)?)?,
                created,
                finish,
                row.get::<Option<u32>>(3)?,
                status,
            ))
        },
    )?;
    let mut entries = Vec::new();
    for (_, created, finish, _, status) in &rows {
        entries.extend([*created].into_iter().chain(*finish));
        entries.extend(messages(status).map(RecordSeq::from));
    }
    entries.sort_unstable();
    entries.dedup();
    let records = decode_records(db, session, Entries::Only(&entries))?;
    let records: HashMap<_, _> = (records.into_iter())
        .map(|record| (record.sequence, record))
        .collect();
    let record = |entry: RecordSeq| {
        (records.get(&entry).cloned())
            .ok_or_else(|| corrupt(format!("job record {entry} is missing")))
    };
    rows.into_iter()
        .map(|(id, created, finish, child, status)| {
            let created = record(created)?;
            let child = child.map(|index| created.agent.child(index));
            let messages = messages(&status)
                .filter_map(|message| Some((message, records.get(&message.into())?.clone())))
                .collect();
            Ok(JobRow {
                id,
                created,
                finish: finish.map(record).transpose()?,
                child,
                status,
                messages,
            })
        })
        .collect()
}

impl Encoder {
    pub(super) fn job(&mut self, db: &Db, entry: Entry, event: &SessionEvent) -> DbResult<()> {
        let seq = entry.seq;
        match event {
            SessionEvent::JobCreated {
                job,
                parent,
                origin,
                tool,
                role,
                name,
                arguments,
                output_schema,
                accepts_input,
                background,
                location,
            } => {
                let call = origin
                    .as_ref()
                    .map(|origin| {
                        db.query_row(
                            "SELECT c.item, c.arguments FROM tool_call c \
                             JOIN assistant_item i ON i.id = c.item \
                             JOIN message_commit m ON m.message = i.message \
                             WHERE m.entry = ?1 AND c.call_id = ?2",
                            params![origin.message, &origin.call_id],
                            |row| Ok((row.get::<i64>(0)?, row.get::<String>(1)?)),
                        )?
                        .ok_or_else(|| rejected("job origin names no committed tool call"))
                    })
                    .transpose()?;
                let arguments = json(arguments)?;
                // Equal text decodes to the same arguments, their key order included.
                let (origin, arguments) = match call {
                    Some((call, called)) if called == arguments => (Some(call), None),
                    call => (call.map(|(call, _)| call), Some(arguments)),
                };
                let output_schema = output_schema
                    .as_deref()
                    .map(|schema| schema_blob(db, schema));
                let target = self.target(db, &location.target)?;
                db.execute(
                    "INSERT INTO job (id, created, parent, origin_call, tool, name, role, \
                     arguments, output_schema, accepts_input, background, location_target, \
                     location_workspace) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                    params![
                        job.get(),
                        seq,
                        parent.map(|parent| parent.get()),
                        origin,
                        tool,
                        name.as_ref(),
                        *role,
                        arguments,
                        output_schema.transpose()?,
                        *accepts_input,
                        *background,
                        target,
                        path_bytes(&location.workspace),
                    ],
                )
                .map(drop)
            }
            SessionEvent::JobStateChanged { job, state } => db
                .execute(
                    "INSERT INTO job_transition (entry, job, state) VALUES (?1, ?2, ?3)",
                    params![seq, job.get(), *state],
                )
                .map(drop),
            SessionEvent::JobFinished {
                job,
                state,
                diagnostic,
                images,
            } => {
                db.execute(
                    "INSERT INTO job_finish (entry, job, state) VALUES (?1, ?2, ?3)",
                    params![seq, job.get(), *state],
                )?;
                if let Some(diagnostic) = diagnostic {
                    self.diagnostic(db, seq, diagnostic)?;
                }
                for (position, image) in images.iter().enumerate() {
                    image_row(db, "job_finish_image", "finish", seq, position, image)?;
                }
                Ok(())
            }
            SessionEvent::JobCancelRequested { job } => db
                .execute(
                    "INSERT INTO job_cancel (entry, job) VALUES (?1, ?2)",
                    params![seq, job.get()],
                )
                .map(drop),
            SessionEvent::JobClaimed { job }
            | SessionEvent::JobReleased { job }
            | SessionEvent::JobMessageReleased { job }
            | SessionEvent::JobInjected { job } => delivery(db, seq, job.get(), None, None),
            SessionEvent::JobMessageDelivered {
                job,
                source,
                notification,
            } => delivery(db, seq, job.get(), Some(*notification), Some(*source)),
            _ => unreachable!("{} is not a job event", event.kind()),
        }
    }
}

/// Store `schema` once in the blob table, returning its digest.
fn schema_blob(db: &Db, schema: &Value) -> DbResult<BlobDigest> {
    let bytes = json(schema)?.into_bytes();
    let digest = BlobDigest::of(&bytes);
    insert_blob(db, digest, &bytes)?;
    Ok(digest)
}

/// Output schemas by digest, each decoded once and shared by every job declaring it.
#[derive(Default)]
struct Schemas(HashMap<BlobDigest, Arc<Value>>);

impl Schemas {
    fn get(&mut self, db: &Db, digest: BlobDigest) -> DbResult<Arc<Value>> {
        if let Some(schema) = self.0.get(&digest) {
            return Ok(schema.clone());
        }
        let bytes = verified_blob(db, digest)?
            .map_err(|reason| corrupt(format!("job output schema {reason}")))?;
        let schema: Arc<Value> =
            Arc::new(serde_json::from_slice(&bytes).map_err(|error| corrupt(error.to_string()))?);
        self.0.insert(digest, schema.clone());
        Ok(schema)
    }
}

fn delivery(
    db: &Db,
    seq: u64,
    job: u64,
    notification: Option<MessageSeq>,
    source: Option<MessageSeq>,
) -> DbResult<()> {
    db.execute(
        "INSERT INTO job_delivery (entry, job, notification, source) VALUES (?1, ?2, ?3, ?4)",
        params![seq, job, notification, source],
    )
    .map(drop)
}

/// The entries that created those of `jobs` the agent of row `agent` created, with
/// their creations, in id order (an id is taken by the commit that creates it).
pub(in crate::session) fn creations(
    db: &Db,
    session: SessionId,
    jobs: &[JobId],
    agent: i64,
) -> DbResult<Vec<(RecordSeq, SessionEvent)>> {
    let jobs: Vec<_> = jobs.iter().map(|job| job.get()).collect();
    let entries = db.query(
        // Driven by the jobs: by the agent, the planner walks its whole history.
        "SELECT j.created FROM job j CROSS JOIN entry e ON e.seq = j.created \
         WHERE j.id IN (SELECT value FROM json_each(?1)) AND e.agent = ?2",
        params![json(&jobs)?, agent],
        entry,
    )?;
    let records = decode_records(db, session, Entries::Only(&entries))?;
    Ok((records.iter())
        .map(|record| (record.sequence, record.event.clone()))
        .collect())
}

/// Job creations, transitions, finishes and deliveries.
pub(super) fn events(events: &mut Events) -> DbResult<()> {
    let db = events.db;
    let mut schemas = Schemas::default();
    let creations = "SELECT j.created, j.id, j.parent, j.origin_call, m.entry, c.call_id, \
         j.tool, j.name, j.role, coalesce(j.arguments, c.arguments), j.output_schema, \
         j.accepts_input, j.background, t.name, j.location_workspace \
         FROM job j JOIN target t ON t.id = j.location_target \
         LEFT JOIN tool_call c ON c.item = j.origin_call \
         LEFT JOIN assistant_item i ON i.id = c.item \
         LEFT JOIN message_commit m ON m.message = i.message {scope}";
    events.load(creations, "j.created", |row| {
        let origin = match (
            row.get::<Option<i64>>(3)?,
            row.get::<Option<i64>>(4)?,
            row.get(5)?,
        ) {
            (None, _, _) => None,
            (Some(_), Some(message), Some(call_id)) => Some(ModelCallOrigin {
                message: sequence(message).message(),
                call_id,
            }),
            _ => return Err(corrupt("job origin call is not committed")),
        };
        Ok(SessionEvent::JobCreated {
            job: job(row.get(1)?)?,
            parent: row.get::<Option<i64>>(2)?.map(job).transpose()?,
            origin,
            tool: row.get(6)?,
            role: enum_column(row, 8)?,
            name: row.get::<Option<String>>(7)?.map(parsed).transpose()?,
            arguments: parse_json(&row.get::<String>(9)?)?,
            output_schema: row
                .get::<Option<Vec<u8>>>(10)?
                .map(|digest| schemas.get(db, BlobDigest::from_bytes(bytes(digest)?)))
                .transpose()?,
            accepts_input: row.get(11)?,
            background: row.get(12)?,
            location: location(row.get(13)?, row.get(14)?)?,
        })
    })?;
    let transitions = "SELECT entry, job, state FROM job_transition {scope}";
    events.load(transitions, "entry", |row| {
        Ok(SessionEvent::JobStateChanged {
            job: job(row.get(1)?)?,
            state: enum_column(row, 2)?,
        })
    })?;
    let diagnostics = diagnostic::load(events)?;
    let images = grouped(
        db,
        events.scoped(
            "SELECT f.finish, f.blob, length(b.bytes), f.format, f.file FROM job_finish_image f \
             JOIN blob b ON b.sha256 = f.blob {scope} ORDER BY f.finish, f.position",
            "f.finish",
        )?,
        |row| image(row, 1),
    )?;
    let finishes = "SELECT entry, job, state FROM job_finish {scope}";
    events.load(finishes, "entry", |row| {
        let seq = row.get::<i64>(0)?;
        Ok(SessionEvent::JobFinished {
            job: job(row.get(1)?)?,
            state: enum_column(row, 2)?,
            diagnostic: diagnostics.get(&seq).cloned(),
            images: images.get(&seq).cloned().unwrap_or_default(),
        })
    })?;
    events.load(
        "SELECT entry, job FROM job_cancel {scope}",
        "entry",
        |row| {
            let job = job(row.get(1)?)?;
            Ok(SessionEvent::JobCancelRequested { job })
        },
    )?;
    events.load(
        "SELECT d.entry, e.kind, d.job, d.notification, d.source FROM job_delivery d \
         JOIN entry e ON e.seq = d.entry {scope}",
        "d.entry",
        |row| {
            let job = job(row.get(2)?)?;
            let message = |at: i32| {
                row.get::<Option<i64>>(at)?
                    .map(|entry| sequence(entry).message())
                    .ok_or_else(|| corrupt("message delivery has no notification or source"))
            };
            Ok(match enum_column(row, 1)? {
                EntryKind::JobClaimed => SessionEvent::JobClaimed { job },
                EntryKind::JobReleased => SessionEvent::JobReleased { job },
                EntryKind::JobMessageReleased => SessionEvent::JobMessageReleased { job },
                EntryKind::JobInjected => SessionEvent::JobInjected { job },
                EntryKind::JobMessageDelivered => SessionEvent::JobMessageDelivered {
                    job,
                    source: message(4)?,
                    notification: message(3)?,
                },
                kind => return Err(corrupt(format!("{kind} entry has a delivery row"))),
            })
        },
    )
}

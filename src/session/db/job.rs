//! Jobs: creation, transitions, finishes with their diagnostics and images, and
//! deliveries to owners.

use super::{
    decode::*,
    diagnostic::{self, Slot},
    encode::*,
    message::{image, image_row},
    *,
};
use crate::{
    execution::path_bytes,
    job::{JobEnd, JobTransition},
    session::{EntryKind, ModelCallOrigin, SessionEvent},
};

impl Encoder {
    pub(super) fn job(&mut self, db: &Db, entry: Entry, event: &SessionEvent) -> DbResult<()> {
        let Entry { seq, kind, .. } = entry;
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
                let origin = origin
                    .as_ref()
                    .map(|origin| {
                        db.query_row(
                            "SELECT c.item FROM tool_call c \
                             JOIN assistant_item i ON i.id = c.item \
                             JOIN message_commit m ON m.message = i.message \
                             WHERE m.entry = ?1 AND c.call_id = ?2",
                            params![origin.message, &origin.call_id],
                            |row| Ok(row.get::<i64>(0)?),
                        )?
                        .ok_or_else(|| rejected("job origin names no committed tool call"))
                    })
                    .transpose()?;
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
                        json(arguments)?,
                        output_schema.as_ref().map(json).transpose()?,
                        *accepts_input,
                        *background,
                        target,
                        path_bytes(&location.workspace),
                    ],
                )?;
                db.execute(
                    "INSERT INTO job_run (job, generation, started) VALUES (?1, 0, ?2)",
                    params![job.get(), seq],
                )
                .map(drop)
            }
            SessionEvent::JobStateChanged { job, state } => {
                db.execute(
                    "INSERT INTO job_transition (entry, job, state) VALUES (?1, ?2, ?3)",
                    params![seq, job.get(), *state],
                )?;
                // A finished job that runs again starts its next generation.
                if *state == JobTransition::Running {
                    db.execute(
                        "INSERT INTO job_run (job, generation, started) \
                         SELECT ?1, (SELECT max(generation) + 1 FROM job_run WHERE job = ?1), ?2 \
                         WHERE (SELECT max(entry) FROM job_finish WHERE job = ?1) > coalesce( \
                           (SELECT max(entry) FROM job_transition WHERE job = ?1 AND entry < ?2), 0)",
                        params![job.get(), seq],
                    )?;
                }
                Ok(())
            }
            SessionEvent::JobFinished {
                job,
                state,
                diagnostic,
                output_diagnostic,
                images,
            } => {
                // Retained jobs reopen after a running reset; only an interrupted
                // outcome may be followed directly by cancellation.
                let previous = db.query_row(
                    "SELECT f.state, f.entry > coalesce((SELECT max(t.entry) FROM job_transition t \
                     WHERE t.job = f.job AND t.state = ?2), 0) \
                     FROM job_finish f WHERE f.job = ?1 ORDER BY f.entry DESC LIMIT 1",
                    params![job.get(), JobTransition::Running],
                    |row| Ok((enum_column::<JobEnd>(row, 0)?, row.get::<bool>(1)?)),
                )?;
                if let Some((previous, current)) = previous
                    && (current || previous == JobEnd::Cancelled)
                {
                    let reopened =
                        current && previous == JobEnd::Interrupted && *state == JobEnd::Cancelled;
                    if !reopened {
                        return Err(rejected(format!(
                            "duplicate or invalid terminal event for job {job}"
                        )));
                    }
                }
                db.execute(
                    "INSERT INTO job_finish (entry, job, state) VALUES (?1, ?2, ?3)",
                    params![seq, job.get(), *state],
                )?;
                for (slot, diagnostic) in [
                    (Slot::Diagnostic, diagnostic),
                    (Slot::OutputDiagnostic, output_diagnostic),
                ] {
                    if let Some(diagnostic) = diagnostic {
                        self.diagnostic(db, seq, slot, diagnostic)?;
                    }
                }
                for (position, image) in images.iter().enumerate() {
                    image_row(db, "job_finish_image", "finish", seq, position, image)?;
                }
                Ok(())
            }
            SessionEvent::JobClaimed { job } | SessionEvent::JobInjected { job } => {
                delivery(db, entry, job.get(), None, None)
            }
            SessionEvent::JobMessageDelivered {
                job,
                source,
                notification,
            } => delivery(db, entry, job.get(), Some(*notification), Some(*source)),
            _ => unreachable!("{kind} is not a job event"),
        }
    }
}

fn delivery(
    db: &Db,
    entry: Entry,
    job: u64,
    notification: Option<MessageSeq>,
    source: Option<MessageSeq>,
) -> DbResult<()> {
    db.execute(
        "INSERT INTO job_delivery (entry, kind, job, notification, source) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![entry.seq, entry.kind, job, notification, source],
    )
    .map(drop)
}

/// Job creations, transitions, finishes and deliveries.
pub(super) fn events(events: &mut Events) -> DbResult<()> {
    let db = events.db;
    events.load(
        "SELECT j.created, j.id, j.parent, j.origin_call, m.entry, c.call_id, j.tool, j.name, \
         j.role, j.arguments, j.output_schema, j.accepts_input, j.background, t.name, \
         j.location_workspace FROM job j \
         JOIN target t ON t.id = j.location_target \
         LEFT JOIN tool_call c ON c.item = j.origin_call \
         LEFT JOIN assistant_item i ON i.id = c.item \
         LEFT JOIN message_commit m ON m.message = i.message",
        |row| {
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
                    .get::<Option<String>>(10)?
                    .as_deref()
                    .map(parse_json)
                    .transpose()?,
                accepts_input: row.get(11)?,
                background: row.get(12)?,
                location: location(row.get(13)?, row.get(14)?)?,
            })
        },
    )?;
    events.load("SELECT entry, job, state FROM job_transition", |row| {
        Ok(SessionEvent::JobStateChanged {
            job: job(row.get(1)?)?,
            state: enum_column(row, 2)?,
        })
    })?;
    let diagnostics = diagnostic::load(db)?;
    let images = grouped(
        db,
        "SELECT f.finish, f.blob, length(b.bytes), f.format, f.file FROM job_finish_image f \
         JOIN blob b ON b.sha256 = f.blob ORDER BY f.finish, f.position",
        |row| image(row, 1),
    )?;
    events.load("SELECT entry, job, state FROM job_finish", |row| {
        let seq = row.get::<i64>(0)?;
        Ok(SessionEvent::JobFinished {
            job: job(row.get(1)?)?,
            state: enum_column(row, 2)?,
            diagnostic: diagnostics.get(&(seq, Slot::Diagnostic)).cloned(),
            output_diagnostic: diagnostics.get(&(seq, Slot::OutputDiagnostic)).cloned(),
            images: images.get(&seq).cloned().unwrap_or_default(),
        })
    })?;
    events.load(
        "SELECT entry, kind, job, notification, source FROM job_delivery",
        |row| {
            let job = job(row.get(2)?)?;
            let message = |at: i32| {
                row.get::<Option<i64>>(at)?
                    .map(|entry| sequence(entry).message())
                    .ok_or_else(|| corrupt("message delivery has no notification or source"))
            };
            Ok(match enum_column(row, 1)? {
                EntryKind::JobClaimed => SessionEvent::JobClaimed { job },
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

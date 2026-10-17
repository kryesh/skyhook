//! Model requests, their attempts and outcomes, turn failures, and compaction.

use std::collections::HashMap;

use super::{
    decode::*,
    encode::*,
    message::{MessageRole, Messages},
    *,
};
use crate::{
    agent::{CompactionFault, Failure, TodoItem},
    provider::protocol::Usage,
    session::{
        AttemptFailure, AttemptRef, CompactionCheckpoint, CompactionFailure, CompactionTrigger,
        CompletedOutcome, SessionEvent,
    },
};

impl Encoder {
    pub(super) fn request(&mut self, db: &Db, entry: Entry, event: &SessionEvent) -> DbResult<()> {
        let seq = entry.seq;
        match event {
            SessionEvent::ModelRequested {
                context,
                checkpoint,
                through,
                from,
                tail,
                history_lifetime,
            } => {
                db.execute(
                    "INSERT INTO model_request (entry, context, checkpoint, history_through, \
                     history_from, history_lifetime) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        seq,
                        *context,
                        *checkpoint,
                        *through,
                        *from,
                        *history_lifetime
                    ],
                )?;
                for (position, message) in tail.iter().enumerate() {
                    let message = self.message(db, message)?;
                    db.execute(
                        "INSERT INTO model_request_tail (request, position, message) \
                         VALUES (?1, ?2, ?3)",
                        params![seq, position, message],
                    )?;
                }
                Ok(())
            }
            SessionEvent::Compaction { checkpoint } => self.compaction(db, seq, checkpoint),
            SessionEvent::ModelAttemptStarted(attempt) => db
                .execute(
                    "INSERT INTO model_attempt (entry, request, attempt) VALUES (?1, ?2, ?3)",
                    params![seq, attempt.request, attempt.attempt],
                )
                .map(drop),
            SessionEvent::ModelFailed { attempt, failure } => {
                outcome(db, seq, *attempt)?;
                let retry = match failure {
                    AttemptFailure::Final(_) => None,
                    AttemptFailure::Retrying { delay, .. } => Some(delay.millis()),
                };
                self::failure(db, seq, &failure.failure(), retry)
            }
            SessionEvent::AgentFailed { failure } => self::failure(db, seq, failure, None),
            SessionEvent::ModelAttemptInterrupted(attempt) => outcome(db, seq, *attempt),
            SessionEvent::ResponseCompleted {
                attempt,
                message,
                outcome: ended,
            } => {
                outcome(db, seq, *attempt)?;
                let message = db
                    .query_row(
                        "SELECT c.message FROM message_commit c \
                         JOIN message m ON m.id = c.message \
                         WHERE c.entry = ?1 AND m.role = ?2",
                        params![*message, MessageRole::Assistant],
                        |row| Ok(row.get::<i64>(0)?),
                    )?
                    .ok_or_else(|| rejected("response message is not a committed message"))?;
                db.execute(
                    "INSERT INTO model_response (entry, message, outcome, cut_reason) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![seq, message, ended.kind(), ended.parts().0],
                )
                .map(drop)
            }
            SessionEvent::CompactionFailed { failure, error } => {
                compaction_failure(db, seq, failure, error)
            }
            SessionEvent::Usage { request, usage } => db
                .execute(
                    "INSERT INTO usage (entry, request, input_tokens, cached_input_tokens, \
                     cache_write_input_tokens, output_tokens) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        seq,
                        *request,
                        usage.input_tokens,
                        usage.cached_input_tokens,
                        usage.cache_write_input_tokens,
                        usage.output_tokens
                    ],
                )
                .map(drop),
            _ => unreachable!("{} is not a request event", event.kind()),
        }
    }

    fn compaction(&mut self, db: &Db, seq: u64, checkpoint: &CompactionCheckpoint) -> DbResult<()> {
        let message = self.message(db, &checkpoint.message)?;
        outcome(db, seq, checkpoint.attempt)?;
        db.execute(
            "INSERT INTO compaction (entry, frontier, message, reported_tokens) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                seq,
                checkpoint.frontier,
                message,
                match checkpoint.trigger {
                    CompactionTrigger::Occupancy(tokens) => Some(tokens),
                    CompactionTrigger::Overflow => None,
                }
            ],
        )?;
        for source in &checkpoint.retained {
            db.execute(
                "INSERT INTO compaction_retained (compaction, source) VALUES (?1, ?2)",
                params![seq, *source],
            )?;
        }
        super::message::todos(db, "todo_item", "entry", seq, &checkpoint.todos)
    }
}

fn failure(db: &Db, seq: u64, failure: &Failure, retry_millis: Option<u64>) -> DbResult<()> {
    let (kind, (detail, class)) = (failure.kind(), failure.parts());
    db.execute(
        "INSERT INTO failure (entry, failure, detail, provider, retry_millis) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![seq, kind, detail, class, retry_millis],
    )
    .map(drop)
}

/// A failure selected as (failure, provider, detail) from column `at`.
pub(super) fn failure_at(row: &Row, at: i32) -> DbResult<Failure> {
    Failure::from_parts(enum_column(row, at)?, row.get(at + 1)?, row.get(at + 2)?)
        .ok_or_else(|| corrupt("failure detail does not match its kind"))
}

fn model_attempt(db: &Db, attempt: AttemptRef) -> DbResult<i64> {
    db.query_row(
        "SELECT entry FROM model_attempt WHERE request = ?1 AND attempt = ?2",
        params![attempt.request, attempt.attempt],
        |row| Ok(row.get::<i64>(0)?),
    )?
    .ok_or_else(|| {
        rejected(format!(
            "request {} has no attempt {}",
            attempt.request, attempt.attempt
        ))
    })
}

/// Record that `attempt` ended with entry `seq`.
fn outcome(db: &Db, seq: u64, attempt: AttemptRef) -> DbResult<()> {
    let attempt = model_attempt(db, attempt)?;
    db.execute(
        "INSERT INTO attempt_outcome (entry, attempt) VALUES (?1, ?2)",
        params![seq, attempt],
    )
    .map(drop)
}

/// A present attempt must exist on its request.
fn compaction_failure(
    db: &Db,
    seq: u64,
    failure: &CompactionFailure,
    fault: &CompactionFault,
) -> DbResult<()> {
    let attempt = failure
        .attempt()
        .map(|attempt| model_attempt(db, attempt))
        .transpose()?;
    let kind = fault.kind();
    let (detail, _) = fault.parts();
    db.execute(
        "INSERT INTO compaction_failure (entry, request, attempt, fault, detail) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![seq, failure.request(), attempt, kind, detail],
    )
    .map(drop)
}

/// The attempt selected as (request, attempt) from column `at`.
fn attempt(row: &Row, at: i32) -> DbResult<AttemptRef> {
    Ok(AttemptRef {
        request: sequence(row.get(at)?).request(),
        attempt: row.get(at + 1)?,
    })
}

/// The attempt the outcome entry `seq` ended.
fn ended(outcomes: &mut HashMap<i64, AttemptRef>, seq: i64) -> DbResult<AttemptRef> {
    (outcomes.remove(&seq)).ok_or_else(|| corrupt(format!("entry {seq} ends no attempt")))
}

/// Requests, attempts and their outcomes, usage, failures and compaction.
pub(super) fn events(
    events: &mut Events,
    messages: &Messages,
    todos: &mut HashMap<i64, Vec<TodoItem>>,
) -> DbResult<()> {
    let db = events.db;
    let tails = grouped(
        db,
        "SELECT request, message FROM model_request_tail ORDER BY request, position",
        |row| messages.message(row.get(1)?),
    )?;
    events.load(
        "SELECT entry, context, checkpoint, history_lifetime, history_through, history_from \
         FROM model_request",
        |row| {
            let seq = row.get::<i64>(0)?;
            Ok(SessionEvent::ModelRequested {
                context: sequence(row.get(1)?),
                checkpoint: row.get::<Option<i64>>(2)?.map(sequence),
                through: row
                    .get::<Option<i64>>(4)?
                    .map(|source| sequence(source).message()),
                from: row
                    .get::<Option<i64>>(5)?
                    .map(|source| sequence(source).message()),
                tail: tails.get(&seq).cloned().unwrap_or_default(),
                history_lifetime: enum_column(row, 3)?,
            })
        },
    )?;
    let retained = grouped(
        db,
        "SELECT compaction, source FROM compaction_retained ORDER BY compaction, source",
        |row| Ok(sequence(row.get(1)?).message()),
    )?;
    // The attempt each outcome ended; what a kind's own row leaves ended interrupted.
    let mut outcomes = keyed(
        db,
        "SELECT o.entry, a.request, a.attempt FROM attempt_outcome o \
         JOIN model_attempt a ON a.entry = o.attempt",
        |row| attempt(row, 1),
    )?;
    events.load(
        "SELECT entry, frontier, message, reported_tokens FROM compaction",
        |row| {
            let seq = row.get::<i64>(0)?;
            Ok(SessionEvent::Compaction {
                checkpoint: CompactionCheckpoint {
                    frontier: sequence(row.get(1)?),
                    message: messages.message(row.get(2)?)?,
                    todos: take(todos, seq),
                    retained: retained.get(&seq).cloned().unwrap_or_default(),
                    attempt: ended(&mut outcomes, seq)?,
                    trigger: match row.get::<Option<u64>>(3)? {
                        Some(tokens) => CompactionTrigger::Occupancy(tokens),
                        None => CompactionTrigger::Overflow,
                    },
                },
            })
        },
    )?;
    events.load("SELECT entry, request, attempt FROM model_attempt", |row| {
        Ok(SessionEvent::ModelAttemptStarted(attempt(row, 1)?))
    })?;
    // A failure that ended an attempt is a model failure; any other, an agent's.
    events.load(
        "SELECT entry, failure, provider, detail, retry_millis FROM failure",
        |row| {
            let attempt = outcomes.remove(&row.get(0)?);
            Ok(
                match (attempt, failure_at(row, 1)?, row.get::<Option<u64>>(4)?) {
                    (None, failure, None) => SessionEvent::AgentFailed { failure },
                    (Some(attempt), failure, None) => SessionEvent::ModelFailed {
                        attempt,
                        failure: AttemptFailure::Final(failure),
                    },
                    (Some(attempt), Failure::Provider(message, kind), Some(millis)) => {
                        let delay = std::time::Duration::from_millis(millis).into();
                        SessionEvent::ModelFailed {
                            attempt,
                            failure: AttemptFailure::Retrying {
                                message,
                                kind,
                                delay,
                            },
                        }
                    }
                    _ => return Err(corrupt("only a model's provider failure retries")),
                },
            )
        },
    )?;
    events.load(
        "SELECT r.entry, m.entry, r.outcome, r.cut_reason FROM model_response r \
         JOIN message_commit m ON m.message = r.message",
        |row| {
            Ok(SessionEvent::ResponseCompleted {
                attempt: ended(&mut outcomes, row.get(0)?)?,
                message: sequence(row.get(1)?).message(),
                outcome: CompletedOutcome::from_parts(enum_column(row, 2)?, None, row.get(3)?)
                    .ok_or_else(|| corrupt("response outcome does not match its cut reason"))?,
            })
        },
    )?;
    for (entry, attempt) in outcomes {
        events.insert(entry, SessionEvent::ModelAttemptInterrupted(attempt))?;
    }
    events.load(
        "SELECT o.entry, o.request, a.attempt, o.fault, o.detail \
         FROM compaction_failure o LEFT JOIN model_attempt a ON a.entry = o.attempt",
        |row| {
            let request = row
                .get::<Option<i64>>(1)?
                .map(|request| sequence(request).request());
            let failure = match (request, row.get::<Option<u64>>(2)?) {
                (None, None) => CompactionFailure::BeforeRequest,
                (Some(request), None) => CompactionFailure::Requested(request),
                (Some(request), Some(attempt)) => {
                    CompactionFailure::Attempted(AttemptRef { request, attempt })
                }
                (None, Some(_)) => return Err(corrupt("compaction attempt has no request")),
            };
            Ok(SessionEvent::CompactionFailed {
                failure,
                error: CompactionFault::from_parts(enum_column(row, 3)?, None, row.get(4)?)
                    .ok_or_else(|| corrupt("compaction fault detail does not match its kind"))?,
            })
        },
    )?;
    events.load(
        "SELECT entry, request, input_tokens, cached_input_tokens, cache_write_input_tokens, \
         output_tokens FROM usage",
        |row| {
            Ok(SessionEvent::Usage {
                request: sequence(row.get(1)?).request(),
                usage: Usage {
                    input_tokens: row.get(2)?,
                    cached_input_tokens: row.get(3)?,
                    cache_write_input_tokens: row.get(4)?,
                    output_tokens: row.get(5)?,
                },
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::FaultKind;
    use crate::session::{
        EntryKind, ModelContext, ModelPurpose,
        db::tests::Fixture,
        tests::{attempt, profile, requested},
    };

    #[test]
    fn compaction_failures_name_their_own_attempt_and_a_fault_matching_its_detail() {
        let mut fixture = Fixture::new();
        let root = fixture.start("/w");
        let context = fixture.one(
            root.clone(),
            SessionEvent::ModelContext {
                context: ModelContext::test(ModelPurpose::Compaction, profile()),
            },
        );
        let mut started = Vec::new();
        for _ in 0..2 {
            let request = fixture.one(root.clone(), requested(context)).request();
            started.push((request, fixture.one(root.clone(), attempt(request, 1))));
        }
        let [(request, own), (_, other)] = started[..] else {
            unreachable!()
        };
        let (checkpoint, stale) = (FaultKind::Checkpoint, Some("stale_todos"));
        let failed = |seq: u64, attempt, detail: Option<&str>| {
            fixture.file(
                seq,
                EntryKind::CompactionFailed,
                &[
                    "INSERT INTO compaction_failure (entry, request, attempt, fault, detail) \
                   VALUES (?1, ?2, ?3, ?4, ?5)",
                ],
                params![request, attempt, checkpoint, detail],
            )
        };
        assert!(failed(100, other, stale).is_err());
        assert!(failed(101, own, Some("not_a_checkpoint_error")).is_err());
        failed(102, own, stale).unwrap();
        fixture.decode().unwrap();
        // A fault missing its detail is stored, but never decodes.
        failed(103, own, None).unwrap();
        assert!(matches!(fixture.decode(), Err(DbError::Corrupt(_))));
    }
}

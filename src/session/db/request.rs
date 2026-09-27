//! Model requests, their attempts and outcomes, turn failures, and compaction.

use std::collections::HashMap;

use super::{
    decode::*,
    encode::*,
    message::{MessageRole, Messages},
    *,
};
use crate::{
    agent::{CompactionFault, Failure, FaultKind, TodoItem},
    named_enum::named_enum,
    provider::protocol::Usage,
    session::{
        AttemptRef, CompactionCheckpoint, CompactionFailure, CompletedOutcome, EntryKind,
        RequestSeq, SessionEvent,
    },
};

named_enum! {
    /// How a completed response ended; `cut` rows also name their `Truncation`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
    pub(super) enum ResponseOutcome {
        Answer = "answer",
        ToolUse = "tool_use",
        Cut = "cut",
    }
}

impl Encoder {
    pub(super) fn request(&mut self, db: &Db, entry: Entry, event: &SessionEvent) -> DbResult<()> {
        let Entry { seq, kind, .. } = entry;
        match event {
            SessionEvent::ModelRequested {
                context,
                checkpoint,
                through,
                tail,
                history_lifetime,
            } => {
                db.execute(
                    "INSERT INTO model_request (entry, context, checkpoint, history_through, \
                     history_lifetime) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![seq, *context, *checkpoint, *through, *history_lifetime],
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
            SessionEvent::Compaction { checkpoint } => self.compaction(db, entry, checkpoint),
            SessionEvent::ModelAttemptStarted(attempt) => db
                .execute(
                    "INSERT INTO model_attempt (entry, request, attempt) VALUES (?1, ?2, ?3)",
                    params![seq, attempt.request, attempt.attempt],
                )
                .map(drop),
            SessionEvent::ModelFailed { attempt, failure } => {
                outcome(db, entry, *attempt)?;
                self::failure(db, entry, failure)
            }
            SessionEvent::AgentFailed { failure } => self::failure(db, entry, failure),
            SessionEvent::ModelAttemptInterrupted(attempt) => outcome(db, entry, *attempt),
            SessionEvent::ResponseCompleted {
                attempt,
                message,
                outcome: ended,
            } => {
                outcome(db, entry, *attempt)?;
                let message = db
                    .query_row(
                        "SELECT c.message FROM message_commit c \
                         JOIN message m ON m.id = c.message \
                         WHERE c.entry = ?1 AND m.role = ?2",
                        params![*message, MessageRole::Assistant],
                        |row| Ok(row.get::<i64>(0)?),
                    )?
                    .ok_or_else(|| rejected("response message is not a committed message"))?;
                let (outcome, cut) = match ended {
                    CompletedOutcome::Answer => (ResponseOutcome::Answer, None),
                    CompletedOutcome::ToolUse => (ResponseOutcome::ToolUse, None),
                    CompletedOutcome::Cut(truncation) => (ResponseOutcome::Cut, Some(*truncation)),
                };
                db.execute(
                    "INSERT INTO model_response (entry, message, outcome, cut_reason) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![seq, message, outcome, cut],
                )
                .map(drop)
            }
            SessionEvent::ModelRecoveryScheduled {
                failure,
                delay_millis,
            } => db
                .execute(
                    "INSERT INTO model_recovery (entry, failure, delay_millis) VALUES (?1, ?2, ?3)",
                    params![seq, *failure, *delay_millis],
                )
                .map(drop),
            SessionEvent::CompactionSkipped { attempt } => {
                compaction_outcome(db, entry, Some(attempt.request), Some(*attempt), None)
            }
            SessionEvent::CompactionFailed { failure, error } => {
                compaction_outcome(db, entry, failure.request(), failure.attempt(), Some(error))
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
            _ => unreachable!("{kind} is not a request event"),
        }
    }

    fn compaction(
        &mut self,
        db: &Db,
        entry: Entry,
        checkpoint: &CompactionCheckpoint,
    ) -> DbResult<()> {
        let message = self.message(db, &checkpoint.message)?;
        outcome(db, entry, checkpoint.attempt)?;
        db.execute(
            "INSERT INTO compaction (entry, frontier, message, before_tokens, after_tokens) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                entry.seq,
                checkpoint.frontier,
                message,
                checkpoint.before_tokens,
                checkpoint.after_tokens
            ],
        )?;
        for source in &checkpoint.retained {
            db.execute(
                "INSERT INTO compaction_retained (compaction, source) VALUES (?1, ?2)",
                params![entry.seq, *source],
            )?;
        }
        super::message::todos(db, entry.seq, entry.kind, &checkpoint.todos)
    }
}

fn failure(db: &Db, entry: Entry, failure: &Failure) -> DbResult<()> {
    let (kind, (detail, class)) = (failure.kind(), failure.parts());
    db.execute(
        "INSERT INTO failure (entry, kind, failure, detailed, detail, provider) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![entry.seq, entry.kind, kind, kind.detailed(), detail, class],
    )
    .map(drop)
}

/// A failure selected as (failure, provider, detail) from column `at`.
fn failure_at(row: &Row, at: i32) -> DbResult<Failure> {
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

/// Record that `attempt` ended with `entry`.
fn outcome(db: &Db, entry: Entry, attempt: AttemptRef) -> DbResult<()> {
    let attempt = model_attempt(db, attempt)?;
    db.execute(
        "INSERT INTO attempt_outcome (entry, kind, attempt) VALUES (?1, ?2, ?3)",
        params![entry.seq, entry.kind, attempt],
    )
    .map(drop)
}

/// A present attempt must exist on `request`.
fn compaction_outcome(
    db: &Db,
    entry: Entry,
    request: Option<RequestSeq>,
    attempt: Option<AttemptRef>,
    fault: Option<&CompactionFault>,
) -> DbResult<()> {
    let attempt = attempt
        .map(|attempt| model_attempt(db, attempt))
        .transpose()?;
    let kind = fault.map(CompactionFault::kind);
    let (detail, _) = fault.map_or((None, None), CompactionFault::parts);
    db.execute(
        "INSERT INTO compaction_outcome (entry, kind, request, attempt, fault, detailed, detail) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            entry.seq,
            entry.kind,
            request,
            attempt,
            kind,
            kind.map(FaultKind::detailed),
            detail
        ],
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

/// Requests, attempts and their outcomes, usage, failures and compaction.
pub(super) fn events(
    events: &mut Events,
    messages: &Messages,
    todos: &HashMap<i64, Vec<TodoItem>>,
) -> DbResult<()> {
    let db = events.db;
    let tails = grouped(
        db,
        "SELECT request, message FROM model_request_tail ORDER BY request, position",
        |row| messages.message(row.get(1)?),
    )?;
    events.load(
        "SELECT entry, context, checkpoint, history_lifetime, history_through FROM model_request",
        |row| {
            let seq = row.get::<i64>(0)?;
            Ok(SessionEvent::ModelRequested {
                context: sequence(row.get(1)?),
                checkpoint: row.get::<Option<i64>>(2)?.map(sequence),
                through: row
                    .get::<Option<i64>>(4)?
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
    events.load(
        "SELECT c.entry, a.request, a.attempt, c.frontier, c.message, c.before_tokens, \
         c.after_tokens FROM compaction c \
         JOIN attempt_outcome o ON o.entry = c.entry JOIN model_attempt a ON a.entry = o.attempt",
        |row| {
            let seq = row.get::<i64>(0)?;
            Ok(SessionEvent::Compaction {
                checkpoint: CompactionCheckpoint {
                    frontier: sequence(row.get(3)?),
                    message: messages.message(row.get(4)?)?,
                    todos: todos.get(&seq).cloned().unwrap_or_default(),
                    retained: retained.get(&seq).cloned().unwrap_or_default(),
                    attempt: attempt(row, 1)?,
                    before_tokens: row.get(5)?,
                    after_tokens: row.get(6)?,
                },
            })
        },
    )?;
    events.load("SELECT entry, request, attempt FROM model_attempt", |row| {
        Ok(SessionEvent::ModelAttemptStarted(attempt(row, 1)?))
    })?;
    events.load_with(
        "SELECT entry, failure, provider, detail FROM failure WHERE kind = ?1",
        params![EntryKind::AgentFailed],
        |row| {
            Ok(SessionEvent::AgentFailed {
                failure: failure_at(row, 1)?,
            })
        },
    )?;
    events.load_with(
        "SELECT f.entry, a.request, a.attempt, f.failure, f.provider, f.detail FROM failure f \
         JOIN attempt_outcome o ON o.entry = f.entry JOIN model_attempt a ON a.entry = o.attempt \
         WHERE f.kind = ?1",
        params![EntryKind::ModelFailed],
        |row| {
            Ok(SessionEvent::ModelFailed {
                attempt: attempt(row, 1)?,
                failure: failure_at(row, 3)?,
            })
        },
    )?;
    events.load_with(
        "SELECT o.entry, a.request, a.attempt FROM attempt_outcome o \
         JOIN model_attempt a ON a.entry = o.attempt WHERE o.kind = ?1",
        params![EntryKind::ModelAttemptInterrupted],
        |row| Ok(SessionEvent::ModelAttemptInterrupted(attempt(row, 1)?)),
    )?;
    events.load(
        "SELECT r.entry, a.request, a.attempt, m.entry, r.outcome, r.cut_reason \
         FROM model_response r JOIN attempt_outcome o ON o.entry = r.entry \
         JOIN model_attempt a ON a.entry = o.attempt \
         JOIN message_commit m ON m.message = r.message",
        |row| {
            Ok(SessionEvent::ResponseCompleted {
                attempt: attempt(row, 1)?,
                message: sequence(row.get(3)?).message(),
                outcome: match (enum_column(row, 4)?, optional_enum_column(row, 5)?) {
                    (ResponseOutcome::Answer, None) => CompletedOutcome::Answer,
                    (ResponseOutcome::ToolUse, None) => CompletedOutcome::ToolUse,
                    (ResponseOutcome::Cut, Some(truncation)) => CompletedOutcome::Cut(truncation),
                    _ => return Err(corrupt("response outcome does not match its cut reason")),
                },
            })
        },
    )?;
    events.load(
        "SELECT entry, failure, delay_millis FROM model_recovery",
        |row| {
            Ok(SessionEvent::ModelRecoveryScheduled {
                failure: sequence(row.get(1)?),
                delay_millis: row.get(2)?,
            })
        },
    )?;
    events.load(
        "SELECT o.entry, o.kind, o.request, a.attempt, o.fault, o.detail \
         FROM compaction_outcome o LEFT JOIN model_attempt a ON a.entry = o.attempt",
        |row| {
            let request = row
                .get::<Option<i64>>(2)?
                .map(|request| sequence(request).request());
            let attempt = match (request, row.get::<Option<u64>>(3)?) {
                (Some(request), Some(attempt)) => Some(AttemptRef { request, attempt }),
                (_, None) => None,
                (None, Some(_)) => return Err(corrupt("compaction attempt has no request")),
            };
            Ok(match enum_column(row, 1)? {
                EntryKind::CompactionSkipped => SessionEvent::CompactionSkipped {
                    attempt: attempt.ok_or_else(|| corrupt("skipped compaction has no attempt"))?,
                },
                EntryKind::CompactionFailed => SessionEvent::CompactionFailed {
                    failure: match (request, attempt) {
                        (None, _) => CompactionFailure::BeforeRequest,
                        (Some(request), None) => CompactionFailure::Requested(request),
                        (Some(_), Some(attempt)) => CompactionFailure::Attempted(attempt),
                    },
                    error: CompactionFault::from_parts(enum_column(row, 4)?, None, row.get(5)?)
                        .ok_or_else(|| {
                            corrupt("compaction fault detail does not match its kind")
                        })?,
                },
                kind => return Err(corrupt(format!("{kind} entry has a compaction outcome"))),
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
    use crate::session::{
        ModelContext, ModelPurpose,
        db::tests::Fixture,
        tests::{attempt, profile, requested},
    };

    #[test]
    fn compaction_outcomes_name_their_own_attempt_and_only_failures_have_a_fault() {
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
        let (checkpoint, stale) = (Some(FaultKind::Checkpoint), Some("stale_todos"));
        for (seq, attempt, fault, detailed, detail, accepted) in [
            (100_u64, other, None, None, None, false),
            (101, own, None, Some(true), stale, false),
            (102, own, checkpoint, Some(true), stale, false),
            (103, own, None, None, None, true),
        ] {
            let kind = EntryKind::CompactionSkipped;
            fixture
                .db
                .execute(
                    "INSERT INTO entry (seq, public_id, agent, created_millis, kind) \
                     VALUES (?1, randomblob(16), (SELECT min(id) FROM agent), 0, ?2)",
                    params![seq, kind],
                )
                .unwrap();
            let inserted = fixture.db.execute(
                "INSERT INTO compaction_outcome \
                 (entry, kind, request, attempt, fault, detailed, detail) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![seq, kind, request, attempt, fault, detailed, detail],
            );
            assert_eq!(inserted.is_ok(), accepted, "{seq}");
        }
    }
}

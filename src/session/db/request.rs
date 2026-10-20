//! Model requests, their attempts and outcomes, turn failures, compaction, and the
//! state rows the request fold writes.

use std::collections::{BTreeMap, HashMap};

use super::{
    contract::profiles,
    decode::*,
    encode::*,
    message::{MessageRole, Messages},
    pages::{paged, paged_sql},
    *,
};
use crate::{
    agent::{CompactionFault, Failure, FailureKind, TodoItem},
    identity::AgentId,
    provider::protocol::Usage,
    session::{
        AttemptFailure, AttemptRef, CompactionCheckpoint, CompactionFailure, CompactionTrigger,
        CompletedOutcome, EntryKind, Page, ProfileSnapshot, RequestPhase, RequestRecord,
        RequestSeq, SessionEvent,
    },
};

crate::named_enum::named_enum! {
    /// A request row's [`RequestPhase`].
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) parsed enum PhaseKind {
        Requested = "requested",
        Open = "open",
        Retrying = "retrying",
        Failed = "failed",
        Interrupted = "interrupted",
        Completed = "completed",
    }
}

/// Write each request's row as the request fold leaves it, in the transaction of the
/// records that changed it.
pub(super) fn write_requests(
    db: &Db,
    requests: &BTreeMap<RequestSeq, RequestRecord>,
) -> DbResult<()> {
    for (request, record) in requests {
        let phase = &record.phase;
        let (kind, message, reported) = match phase {
            RequestPhase::Requested => (PhaseKind::Requested, None, None),
            RequestPhase::Open { message, usage, .. } => (PhaseKind::Open, *message, *usage),
            RequestPhase::Retrying { .. } => (PhaseKind::Retrying, None, None),
            RequestPhase::Failed { .. } => (PhaseKind::Failed, None, None),
            RequestPhase::Interrupted { .. } => (PhaseKind::Interrupted, None, None),
            RequestPhase::Completed { usage, .. } => (PhaseKind::Completed, None, *usage),
        };
        let (usage, counts) = (record.usage, reported.unwrap_or_default());
        db.execute(
            "INSERT OR REPLACE INTO request_state (request, phase, phased, attempts, attempt, \
               message, input_tokens, cached_input_tokens, cache_write_input_tokens, \
               output_tokens, reported, reported_input_tokens, reported_cached_input_tokens, \
               reported_cache_write_input_tokens, reported_output_tokens) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                *request,
                kind,
                record.phased,
                record.attempts,
                phase.last_attempt(),
                message,
                usage.input_tokens,
                usage.cached_input_tokens,
                usage.cache_write_input_tokens,
                usage.output_tokens,
                reported.is_some(),
                counts.input_tokens,
                counts.cached_input_tokens,
                counts.cache_write_input_tokens,
                counts.output_tokens
            ],
        )?;
    }
    Ok(())
}

/// The usage counts selected from column `at` on.
pub(super) fn usage_at(row: &Row, at: i32) -> DbResult<Usage> {
    Ok(Usage {
        input_tokens: row.get(at)?,
        cached_input_tokens: row.get(at + 1)?,
        cache_write_input_tokens: row.get(at + 2)?,
        output_tokens: row.get(at + 3)?,
    })
}

/// What a row's open or completed attempt reported, selected as `reported` and its
/// counts from column `at` on.
pub(super) fn reported_at(row: &Row, at: i32) -> DbResult<Option<Usage>> {
    let reported = row.get::<bool>(at)?;
    reported.then(|| usage_at(row, at + 1)).transpose()
}

/// Requests' rows `s`, each with its requesting entry `e`, filtered by `$filter`, as
/// [`request_record`] reads them.
macro_rules! request_rows {
    ($filter:literal) => {
        concat!(
            "SELECT e.seq, e.created_millis, r.context, c.purpose, c.profile, r.history_through, \
               s.phase, s.phased, p.created_millis, s.attempts, s.attempt, s.message, \
               f.failure, f.provider, f.detail, f.retry_millis, x.fault, x.detail, \
               s.input_tokens, s.cached_input_tokens, s.cache_write_input_tokens, \
               s.output_tokens, s.reported, s.reported_input_tokens, \
               s.reported_cached_input_tokens, s.reported_cache_write_input_tokens, \
               s.reported_output_tokens, e.agent \
             FROM entry e JOIN request_state s ON s.request = e.seq \
             JOIN model_request r ON r.entry = e.seq JOIN model_context c ON c.entry = r.context \
             JOIN entry p ON p.seq = s.phased LEFT JOIN failure f ON f.entry = s.phased \
             LEFT JOIN compaction_failure x ON x.entry = s.phased ",
            $filter
        )
    };
}

/// Up to `limit` of `agent`'s requests at `page`, as the request fold leaves them.
pub(in crate::session) fn requests(
    db: &Db,
    agent: &AgentId,
    page: Page<RequestSeq>,
    limit: usize,
) -> DbResult<Vec<(RequestSeq, RequestRecord)>> {
    let Some(row) = agent_row(db, agent)? else {
        return Ok(Vec::new());
    };
    let profiles = profiles(db)?;
    paged(
        db,
        page,
        paged_sql!(
            request_rows!(""),
            "e.agent = ?3 AND e.kind = ?4",
            "e.seq",
            "?1"
        ),
        params![row, EntryKind::ModelRequested],
        limit,
        |row| request_record(row, agent, &profiles),
    )
}

/// `agent`'s requests at `requests`, as the request fold leaves them.
pub(super) fn requests_at(
    db: &Db,
    agent: &AgentId,
    requests: &[RecordSeq],
) -> DbResult<BTreeMap<RequestSeq, RequestRecord>> {
    if requests.is_empty() {
        return Ok(BTreeMap::new());
    }
    let profiles = profiles(db)?;
    let sql = request_rows!("WHERE e.seq IN (SELECT value FROM json_each(?1))");
    let rows = db.query(sql, params![json(&requests)?], |row| {
        request_record(row, agent, &profiles)
    })?;
    Ok(rows.into_iter().collect())
}

/// Which request of each agent its state row names.
#[derive(Clone, Copy)]
pub(super) enum Named {
    Latest,
    Gauged,
}

/// The request of each agent `agents` names by row that its state row names `which`,
/// as the request fold leaves it.
pub(super) fn named_requests(
    db: &Db,
    agents: &HashMap<i64, AgentId>,
    which: Named,
) -> DbResult<Vec<(RequestSeq, RequestRecord)>> {
    let profiles = profiles(db)?;
    let sql = match which {
        Named::Latest => request_rows!("JOIN agent_state g ON g.request = e.seq"),
        Named::Gauged => request_rows!("JOIN agent_state g ON g.gauged = e.seq"),
    };
    db.query(sql, Vec::new(), |row| {
        let agent = agents.get(&row.get::<i64>(27)?);
        let agent = agent.ok_or_else(|| corrupt("a request names no agent"))?;
        request_record(row, agent, &profiles)
    })
}

/// A request of `agent` selected by [`request_rows`].
fn request_record(
    row: &Row,
    agent: &AgentId,
    profiles: &HashMap<i64, ProfileSnapshot>,
) -> DbResult<(RequestSeq, RequestRecord)> {
    let unlike = || corrupt("a request's row does not match its phase");
    let (attempt, at) = (row.get::<Option<u64>>(10)?, row.get::<i64>(8)?);
    let reported = reported_at(row, 22)?;
    let failure = || match optional_enum_column::<FailureKind>(row, 12)? {
        Some(_) => Ok(failure_at(row, 12)?.into()),
        None => CompactionFault::from_parts(enum_column(row, 16)?, None, row.get(17)?)
            .map(Into::into)
            .ok_or_else(unlike),
    };
    let phase = match enum_column(row, 6)? {
        PhaseKind::Requested => RequestPhase::Requested,
        PhaseKind::Open => RequestPhase::Open {
            attempt: attempt.ok_or_else(unlike)?,
            message: row
                .get::<Option<i64>>(11)?
                .map(|commit| sequence(commit).message()),
            usage: reported,
        },
        PhaseKind::Retrying => match (failure_at(row, 12)?, row.get::<Option<u64>>(15)?) {
            (Failure::Provider(message, kind), Some(delay)) => RequestPhase::Retrying {
                attempt: attempt.ok_or_else(unlike)?,
                failure: kind.error(message),
                due: at.saturating_add_unsigned(delay),
            },
            _ => return Err(unlike()),
        },
        PhaseKind::Failed => RequestPhase::Failed {
            attempt,
            failure: failure()?,
            at,
        },
        PhaseKind::Interrupted => RequestPhase::Interrupted { attempt, at },
        PhaseKind::Completed => RequestPhase::Completed {
            attempt: attempt.ok_or_else(unlike)?,
            at,
            usage: reported,
        },
    };
    let profile = profiles.get(&row.get(4)?);
    let record = RequestRecord {
        agent: agent.clone(),
        context: sequence(row.get(2)?),
        purpose: enum_column(row, 3)?,
        profile: profile
            .cloned()
            .ok_or_else(|| corrupt("a context names no profile"))?,
        through: row
            .get::<Option<i64>>(5)?
            .map(|through| sequence(through).message()),
        requested_millis: row.get(1)?,
        attempts: row.get(9)?,
        usage: usage_at(row, 18)?,
        phase,
        phased: sequence(row.get(7)?),
    };
    Ok((sequence(row.get(0)?).request(), record))
}

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
        events.scoped(
            "SELECT request, message FROM model_request_tail {scope} ORDER BY request, position",
            "request",
        )?,
        |row| messages.message(row.get(1)?),
    )?;
    events.load(
        "SELECT entry, context, checkpoint, history_lifetime, history_through, history_from \
         FROM model_request {scope}",
        "entry",
        |row| {
            Ok(SessionEvent::ModelRequested {
                context: sequence(row.get(1)?),
                checkpoint: row.get::<Option<i64>>(2)?.map(sequence),
                through: row
                    .get::<Option<i64>>(4)?
                    .map(|source| sequence(source).message()),
                from: row
                    .get::<Option<i64>>(5)?
                    .map(|source| sequence(source).message()),
                tail: tails.get(&row.get::<i64>(0)?).cloned().unwrap_or_default(),
                history_lifetime: enum_column(row, 3)?,
            })
        },
    )?;
    let retained = grouped(
        db,
        events.scoped(
            "SELECT compaction, source FROM compaction_retained {scope} \
             ORDER BY compaction, source",
            "compaction",
        )?,
        |row| Ok(sequence(row.get(1)?).message()),
    )?;
    // The attempt each outcome ended; what a kind's own row leaves ended interrupted.
    let mut outcomes = keyed(
        db,
        events.scoped(
            "SELECT o.entry, a.request, a.attempt FROM attempt_outcome o \
             JOIN model_attempt a ON a.entry = o.attempt {scope}",
            "o.entry",
        )?,
        |row| attempt(row, 1),
    )?;
    events.load(
        "SELECT entry, frontier, message, reported_tokens FROM compaction {scope}",
        "entry",
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
    let attempts = "SELECT entry, request, attempt FROM model_attempt {scope}";
    events.load(attempts, "entry", |row| {
        Ok(SessionEvent::ModelAttemptStarted(attempt(row, 1)?))
    })?;
    // A failure that ended an attempt is a model failure; any other, an agent's.
    events.load(
        "SELECT entry, failure, provider, detail, retry_millis FROM failure {scope}",
        "entry",
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
         JOIN message_commit m ON m.message = r.message {scope}",
        "r.entry",
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
         FROM compaction_failure o LEFT JOIN model_attempt a ON a.entry = o.attempt {scope}",
        "o.entry",
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
         output_tokens FROM usage {scope}",
        "entry",
        |row| {
            Ok(SessionEvent::Usage {
                request: sequence(row.get(1)?).request(),
                usage: usage_at(row, 2)?,
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

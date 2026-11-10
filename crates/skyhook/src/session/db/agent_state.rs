//! The agents' current-state rows the agent fold writes, and reading them back.

use std::collections::HashMap;

use libsql::Row;

use super::{
    Db, DbResult, Encoder, corrupt,
    decode::*,
    optional_enum_column, params,
    requests::{failure_at, reported_at},
};
use crate::identity::AgentId;
use crate::{
    provider::protocol::UsageTotals,
    session::{
        HostCursor, ResidentLedger,
        agent_state::{AgentState, Decided, Last, LastKind, Marked, Outcome, OutcomeKind, Rows},
    },
};

/// The profile of the agent row `s` applies: its `profiled` entry's.
pub(super) const PROFILED: &str = "coalesce( \
    (SELECT profile FROM model_selection WHERE entry = s.profiled), \
    (SELECT profile FROM agent_start WHERE entry = s.profiled))";

/// The [`Last`] whose kind and failure are columns `at` on: the kind, then the failure's
/// kind, provider and detail.
pub(super) fn last_at(row: &Row, at: i32) -> DbResult<Option<Last>> {
    Ok(match optional_enum_column::<LastKind>(row, at)? {
        None => None,
        Some(LastKind::Awaiting) => Some(Last::Awaiting),
        Some(LastKind::Answered) => Some(Last::Answered),
        Some(LastKind::Failed) => Some(Last::Failed(failure_at(row, at + 1)?)),
        Some(LastKind::Refused) => Some(Last::Refused(failure_at(row, at + 1)?)),
    })
}

/// The [`Outcome`] whose kind and failure are columns `at` on, as for [`last_at`].
pub(super) fn outcome_at(row: &Row, at: i32) -> DbResult<Option<Outcome>> {
    Ok(match optional_enum_column::<OutcomeKind>(row, at)? {
        None => None,
        Some(OutcomeKind::Completed) => Some(Outcome::Completed),
        Some(OutcomeKind::Interrupted) => Some(Outcome::Interrupted),
        Some(OutcomeKind::Failed) => Some(Outcome::Failed(failure_at(row, at + 1)?)),
    })
}

/// The fact columns `at` and `at + 1` mark: its entry and when that was journaled.
fn marked<T>(value: Option<T>, row: &Row, at: i32) -> DbResult<Option<Marked<T>>> {
    value
        .map(|value| {
            Ok(Marked {
                value,
                entry: sequence(row.get(at)?),
                millis: row.get(at + 1)?,
            })
        })
        .transpose()
}

impl Encoder {
    /// Write each changed request's and agent's row, each new model context's and what
    /// the fold decided, as the folds leave them, in the transaction of the records that
    /// changed them.
    pub(in crate::session) fn write_rows(
        &mut self,
        db: &Db,
        rows: &Rows,
        ledger: &ResidentLedger,
    ) -> DbResult<()> {
        super::requests::write_requests(db, &rows.requests)?;
        for (agent, state) in &rows.agents {
            let row = self.agent(db, agent)?;
            let last = (state.last.as_ref()).map(|last| last.value.kind());
            let outcome = (state.outcome.as_ref()).map(|outcome| outcome.value.kind());
            let usage = state.usage;
            db.execute(
                "INSERT OR REPLACE INTO agent_state (agent, profiled, granted, todos, checkpoint, \
                   request, awaiting, occupancy, gauged, last, last_entry, outcome, outcome_entry, \
                   turn, ended, finished, finished_entry, input_tokens, cached_input_tokens, \
                   cache_write_input_tokens, output_tokens, host_events) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, \
                   ?17, ?18, ?19, ?20, ?21, ?22)",
                params![
                    row,
                    state.profiled,
                    state.granted,
                    state.todos,
                    state.checkpoint,
                    ledger.latest(agent),
                    ledger.awaits(agent),
                    state.reported.map(|(request, _)| request),
                    state.gauged,
                    last,
                    state.last.as_ref().map(|last| last.entry),
                    outcome,
                    state.outcome.as_ref().map(|outcome| outcome.entry),
                    state.turn,
                    state.ended,
                    state.finished.map(|(turn, _)| turn),
                    state.finished.map(|(_, ended)| ended),
                    usage.input_tokens,
                    usage.cached_input_tokens,
                    usage.cache_write_input_tokens,
                    usage.output_tokens,
                    state.host_events.map(|(message, _)| message)
                ],
            )?;
        }
        for (agent, purpose, context) in &rows.contexts {
            let row = self.agent(db, agent)?;
            db.execute(
                "INSERT OR REPLACE INTO agent_context (agent, purpose, context) VALUES (?1, ?2, ?3)",
                params![row, *purpose, *context],
            )?;
        }
        for decided in &rows.decided {
            match *decided {
                Decided::Answer {
                    answer,
                    request,
                    turn,
                } => db.execute(
                    "INSERT INTO answer_turn (answer, request, turn) VALUES (?1, ?2, ?3)",
                    params![answer, request, turn],
                )?,
                Decided::Failed { turn } => {
                    db.execute("INSERT INTO failed_turn (entry) VALUES (?1)", params![turn])?
                }
                Decided::Retried {
                    request,
                    first,
                    last,
                } => db.execute(
                    "INSERT OR REPLACE INTO retried_run (request, first, last) VALUES (?1, ?2, ?3)",
                    params![request, first, last],
                )?,
            };
        }
        Ok(())
    }
}

/// Every started agent's state, as its row holds it; `agents` names them by row.
pub(in crate::session) fn agent_states(
    db: &Db,
    agents: &HashMap<i64, AgentId>,
) -> DbResult<HashMap<AgentId, AgentState>> {
    let mut capabilities = grouped(
        db,
        "SELECT s.agent, c.capability FROM agent_state s \
         JOIN agent_capability c ON c.entry = s.granted",
        |row| parsed(row.get(1)?),
    )?;
    let rows = db.query(
        "SELECT s.agent, a.available_depth, s.profiled, s.granted, s.todos, s.checkpoint, \
           s.occupancy, o.reported, o.reported_input_tokens, o.reported_cached_input_tokens, \
           o.reported_cache_write_input_tokens, o.reported_output_tokens, s.gauged, s.last, \
           lf.failure, lf.provider, lf.detail, s.last_entry, l.created_millis, s.outcome, \
           xf.failure, xf.provider, xf.detail, s.outcome_entry, x.created_millis, s.turn, \
           s.ended, s.finished, s.finished_entry, s.input_tokens, s.cached_input_tokens, \
           s.cache_write_input_tokens, s.output_tokens, s.host_events, \
           (SELECT h.cursor FROM message_commit hm JOIN user_part hp ON hp.message = hm.message \
             JOIN user_part_job_event h ON h.part = hp.id AND h.kind = 'host' \
             WHERE hm.entry = s.host_events) \
         FROM agent_state s JOIN agent a ON a.id = s.agent \
         LEFT JOIN request_state o ON o.request = s.occupancy \
         LEFT JOIN entry l ON l.seq = s.last_entry LEFT JOIN failure lf ON lf.entry = s.last_entry \
         LEFT JOIN entry x ON x.seq = s.outcome_entry \
         LEFT JOIN failure xf ON xf.entry = s.outcome_entry",
        Vec::new(),
        |row| {
            let optional = |at| Ok::<_, super::DbError>(row.get::<Option<i64>>(at)?.map(sequence));
            let request = |at| Ok::<_, super::DbError>(optional(at)?.map(|seq| seq.request()));
            let unreported = || corrupt("an agent's occupancy names no reported request");
            let reported = match request(6)? {
                Some(request) => Some((request, reported_at(row, 7)?.ok_or_else(unreported)?)),
                None => None,
            };
            let state = AgentState {
                profiled: sequence(row.get(2)?),
                granted: sequence(row.get(3)?),
                capabilities: Default::default(),
                available_depth: usize::try_from(row.get::<i64>(1)?).unwrap_or_default(),
                todos: optional(4)?,
                checkpoint: optional(5)?,
                host_events: match (optional(33)?, row.get::<Option<i64>>(34)?) {
                    (Some(message), Some(through)) => Some((
                        message,
                        HostCursor::try_from(through)
                            .map_err(|error| corrupt(error.to_string()))?,
                    )),
                    (None, None) => None,
                    _ => return Err(corrupt("an agent's host events name no host event")),
                },
                reported,
                gauged: request(12)?,
                last: marked(last_at(row, 13)?, row, 17)?,
                outcome: marked(outcome_at(row, 19)?, row, 23)?,
                turn: request(25)?,
                ended: optional(26)?,
                finished: request(27)?.zip(optional(28)?),
                usage: UsageTotals {
                    input_tokens: u64_of(row.get(29)?),
                    cached_input_tokens: u64_of(row.get(30)?),
                    cache_write_input_tokens: u64_of(row.get(31)?),
                    output_tokens: u64_of(row.get(32)?),
                },
            };
            Ok((row.get::<i64>(0)?, state))
        },
    )?;
    (rows.into_iter())
        .map(|(row, mut state)| {
            let agent = agents.get(&row).cloned();
            let agent = agent.ok_or_else(|| corrupt("an agent's state names no agent"))?;
            let granted = capabilities.remove(&row).unwrap_or_default();
            state.capabilities = granted.into_iter().collect();
            Ok((agent, state))
        })
        .collect()
}

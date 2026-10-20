//! Current state: the journal's counters, the session list's summary and title, and the
//! agents' rows the agent fold writes.

use std::{collections::HashMap, sync::LazyLock};

use libsql::Row;

use super::{
    Db, DbResult, Encoder, corrupt,
    decode::*,
    enum_column,
    message::{MessageRole, UserPartKind},
    optional_enum_column, params,
    request::{PhaseKind, failure_at, reported_at},
};
use crate::{
    agent::TurnFailure,
    identity::AgentId,
    provider::{profile::ModelRef, protocol::UsageTotals},
    session::{
        EntryKind, ResidentLedger, TitleSource,
        agent_state::{AgentState, Decided, Last, Marked, Outcome, Rows, stopped},
        core::Counters,
    },
};

crate::named_enum::named_enum! {
    /// An agent row's [`Last`].
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) parsed enum LastKind {
        Awaiting = "awaiting",
        Answered = "answered",
        Failed = "failed",
        Refused = "refused",
    }
}

crate::named_enum::named_enum! {
    /// An agent row's [`Outcome`].
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) parsed enum OutcomeKind {
        Completed = "completed",
        Interrupted = "interrupted",
        Failed = "failed",
    }
}

/// The profile of the agent row `s` applies: its `profiled` entry's.
pub(super) const PROFILED: &str = "coalesce( \
    (SELECT profile FROM model_selection WHERE entry = s.profiled), \
    (SELECT profile FROM agent_start WHERE entry = s.profiled))";

/// A session list row, read without decoding the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSummary {
    /// The user's title, else the newest automatic one, else the first text the
    /// user sent the root agent, as a `Prompt` title.
    pub title: Option<SessionTitle>,
    /// When the session last did something: its newest record that
    /// [`is_activity`](crate::session::SessionEvent::is_activity).
    pub last_millis: i64,
    /// How many entries the journal holds.
    pub entries: u64,
    /// The root agent's applied model.
    pub model: Option<ModelRef>,
    /// The root agent's applied mode.
    pub mode: Option<crate::tool::policy::ModeName>,
    /// The root agent's stopped turn, which resuming continues, if any. A root
    /// that closed idle with deliveries pending has none, yet resumes interrupted.
    pub stopped: Option<TurnFailure>,
}

/// A session's title and where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionTitle {
    pub text: String,
    pub source: TitleSource,
}

/// The first text the user sent the root agent, which [`root_prompt_params`] binds.
const ROOT_PROMPT: &str = "\
    SELECT p.text FROM message_commit mc
      JOIN entry e ON e.seq = mc.entry AND e.kind = ?1
      JOIN agent a ON a.id = e.agent AND a.parent IS NULL
      JOIN message m ON m.id = mc.message AND m.role = ?2
      JOIN user_part p ON p.message = mc.message AND p.kind = ?3
     ORDER BY mc.entry, p.position LIMIT 1";

/// The session's title row, if any: the user's newest title, unless a title_cleared
/// follows it; else the newest automatic one; else the [`ROOT_PROMPT`], as a prompt
/// title. Like every query here, it reads a subtype row only under an entry of its
/// kind. `?1`-`?3` bind as for [`ROOT_PROMPT`], `?4` and `?5` the title_set and
/// title_cleared kinds.
static TITLE: LazyLock<String> = LazyLock::new(|| {
    format!(
        "WITH titles AS (
           SELECT t.* FROM title t JOIN entry e ON e.seq = t.entry AND e.kind = ?4),
         chosen(entry) AS (
           SELECT coalesce(
             (SELECT max(u.entry) FROM titles u
               WHERE u.source = 'user' AND u.entry > coalesce(
                 (SELECT max(e.seq) FROM entry e
                   WHERE e.agent = (SELECT id FROM agent WHERE parent IS NULL)
                     AND e.kind = ?5),
                 0)),
             (SELECT max(a.entry) FROM titles a WHERE a.source <> 'user')))
         SELECT t.text, t.source FROM titles t WHERE t.entry = (SELECT entry FROM chosen)
         UNION ALL
         SELECT text, 'prompt' FROM ({ROOT_PROMPT}) WHERE (SELECT entry FROM chosen) IS NULL"
    )
});

fn root_prompt_params() -> Vec<libsql::Value> {
    params![
        EntryKind::MessageCommitted,
        MessageRole::User,
        UserPartKind::Text
    ]
}

/// The first text the user sent the root agent.
pub(in crate::session) fn root_prompt(db: &Db) -> DbResult<Option<String>> {
    db.query_row(ROOT_PROMPT, root_prompt_params(), |row| Ok(row.get(0)?))
}

/// The parameters of [`TITLE`].
fn title_params() -> Vec<libsql::Value> {
    let mut params = root_prompt_params();
    params.extend(params![EntryKind::TitleSet, EntryKind::TitleCleared]);
    params
}

/// When the newest entry that is the session's activity was journaled, if any.
const LAST_ACTIVITY: &str = "\
    SELECT e.created_millis FROM entry e JOIN entry_kind k ON k.name = e.kind \
     WHERE k.activity ORDER BY e.seq DESC LIMIT 1";

/// What the journal's newest entries leave, which the core counts on from there.
pub(in crate::session) fn counters(db: &Db) -> DbResult<Counters> {
    let counters = db.query_row(
        format!("SELECT coalesce(max(seq), 0), ({LAST_ACTIVITY}) FROM entry"),
        Vec::new(),
        |row| {
            Ok(Counters {
                newest: sequence(row.get(0)?),
                last_activity: row.get(1)?,
            })
        },
    )?;
    counters.ok_or_else(|| corrupt("journal counters are missing"))
}

/// Always one row, whether or not the session has a title or any activity.
pub(in crate::session) fn summary(db: &Db) -> DbResult<SessionSummary> {
    let root = db.query_row(
        format!(
            "SELECT p.provider, p.name, (SELECT mode_name FROM agent_mode WHERE entry = s.granted), \
               s.last, f.failure, f.provider, f.detail, \
               (SELECT phase FROM request_state WHERE request = s.request) \
             FROM agent_state s JOIN agent a ON a.id = s.agent AND a.parent IS NULL \
             JOIN model_profile p ON p.id = {PROFILED} LEFT JOIN failure f ON f.entry = s.last_entry"
        ),
        Vec::new(),
        |row| {
            let model = model_ref(&row.get::<String>(0)?, &row.get::<String>(1)?)?;
            let mode = row.get::<Option<String>>(2)?.map(parsed).transpose()?;
            let last = last_at(row, 3)?;
            let phase = optional_enum_column::<PhaseKind>(row, 7)?;
            let incomplete = phase.is_some_and(|phase| phase != PhaseKind::Completed);
            Ok((model, mode, stopped(last.as_ref(), incomplete)))
        },
    )?;
    let (model, mode, stopped) = match root {
        Some((model, mode, stopped)) => (Some(model), mode, stopped),
        None => (None, None, None),
    };
    db.query_row(
        format!(
            "SELECT t.text, t.source, coalesce(({LAST_ACTIVITY}), 0), \
               (SELECT coalesce(max(seq), 0) FROM entry) FROM (SELECT 1) LEFT JOIN ({}) t ON true",
            *TITLE
        ),
        title_params(),
        |row| {
            Ok(SessionSummary {
                title: title_at(row, 0)?,
                last_millis: row.get(2)?,
                entries: row.get(3)?,
                model: model.clone(),
                mode: mode.clone(),
                stopped: stopped.clone(),
            })
        },
    )?
    .ok_or_else(|| corrupt("session summary is missing"))
}

/// The session list's title.
pub(in crate::session) fn title(db: &Db) -> DbResult<Option<SessionTitle>> {
    let title = db.query_row(TITLE.as_str(), title_params(), |row| title_at(row, 0))?;
    Ok(title.flatten())
}

/// The title whose text and source are columns `at` and `at + 1`, both NULL for none.
fn title_at(row: &Row, at: i32) -> DbResult<Option<SessionTitle>> {
    let Some(text) = row.get::<Option<String>>(at)? else {
        return Ok(None);
    };
    let source = enum_column(row, at + 1)?;
    Ok(Some(SessionTitle { text, source }))
}

/// The [`Last`] whose kind and failure are columns `at` on: the kind, then the failure's
/// kind, provider and detail.
fn last_at(row: &Row, at: i32) -> DbResult<Option<Last>> {
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
        super::request::write_requests(db, &rows.requests)?;
        for (agent, state) in &rows.agents {
            let row = self.agent(db, agent)?;
            let last = (state.last.as_ref()).map(|last| match &last.value {
                Last::Awaiting => LastKind::Awaiting,
                Last::Answered => LastKind::Answered,
                Last::Failed(_) => LastKind::Failed,
                Last::Refused(_) => LastKind::Refused,
            });
            let outcome = (state.outcome.as_ref()).map(|outcome| match &outcome.value {
                Outcome::Completed => OutcomeKind::Completed,
                Outcome::Interrupted => OutcomeKind::Interrupted,
                Outcome::Failed(_) => OutcomeKind::Failed,
            });
            let usage = state.usage;
            db.execute(
                "INSERT OR REPLACE INTO agent_state (agent, profiled, granted, todos, checkpoint, \
                   request, awaiting, occupancy, gauged, last, last_entry, outcome, outcome_entry, \
                   turn, ended, finished, finished_entry, input_tokens, cached_input_tokens, \
                   cache_write_input_tokens, output_tokens) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, \
                   ?17, ?18, ?19, ?20, ?21)",
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
                    usage.output_tokens
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
            };
        }
        Ok(())
    }
}

/// Every fact the fold decided of a record, as the rows hold them.
#[cfg(debug_assertions)]
pub(in crate::session) fn decided(db: &Db) -> DbResult<Vec<Decided>> {
    let mut decided = db.query(
        "SELECT answer, request, turn FROM answer_turn",
        Vec::new(),
        |row| {
            Ok(Decided::Answer {
                answer: sequence(row.get(0)?),
                request: sequence(row.get(1)?).request(),
                turn: row
                    .get::<Option<i64>>(2)?
                    .map(|turn| sequence(turn).request()),
            })
        },
    )?;
    decided.extend(
        db.query("SELECT entry FROM failed_turn", Vec::new(), |row| {
            Ok(Decided::Failed {
                turn: sequence(row.get(0)?),
            })
        })?,
    );
    Ok(decided)
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
        |row| enum_column(row, 1),
    )?;
    let rows = db.query(
        "SELECT s.agent, a.available_depth, s.profiled, s.granted, s.todos, s.checkpoint, \
           s.occupancy, o.reported, o.reported_input_tokens, o.reported_cached_input_tokens, \
           o.reported_cache_write_input_tokens, o.reported_output_tokens, s.gauged, s.last, \
           lf.failure, lf.provider, lf.detail, s.last_entry, l.created_millis, s.outcome, \
           xf.failure, xf.provider, xf.detail, s.outcome_entry, x.created_millis, s.turn, \
           s.ended, s.finished, s.finished_entry, s.input_tokens, s.cached_input_tokens, \
           s.cache_write_input_tokens, s.output_tokens \
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

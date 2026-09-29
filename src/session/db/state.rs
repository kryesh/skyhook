//! Current-state queries over the schema's views.

use libsql::Row;

use super::{
    Db, DbResult, corrupt, enum_column, message::MessageRole, params, request::failure_at,
};
use crate::agent::TurnFailure;
use crate::provider::profile::ModelRef;
use crate::session::{EntryKind, TitleSource};

/// A session list row, read without decoding the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSummary {
    /// The user's title, else the newest automatic one, else the first text the
    /// user sent the root agent, as a `Prompt` title.
    pub title: Option<SessionTitle>,
    /// When the session last did something: its newest record that
    /// [`is_activity`](crate::session::SessionEvent::is_activity).
    pub last_millis: i64,
    pub entries: u64,
    /// The root agent's applied model.
    pub model: Option<ModelRef>,
    /// The root agent's applied mode.
    pub mode: Option<crate::tool::policy::ModeName>,
    /// How the root agent would resume: the turn it would continue, if any.
    pub stopped: Option<TurnFailure>,
}

/// A session's title and where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionTitle {
    pub text: String,
    pub source: TitleSource,
}

pub(in crate::session) fn summary(db: &Db) -> DbResult<SessionSummary> {
    let stopped = stopped_turn(db)?;
    db.query_row(
        "SELECT t.text, t.source, coalesce(last_millis, 0), entries, p.provider, p.name, mode \
         FROM session_summary s LEFT JOIN session_title t ON true \
         LEFT JOIN model_profile p ON p.id = s.profile",
        Vec::new(),
        |row| {
            let model = match (row.get::<Option<String>>(4)?, row.get::<Option<String>>(5)?) {
                (Some(provider), Some(name)) => Some(super::decode::model_ref(&provider, &name)?),
                _ => None,
            };
            Ok(SessionSummary {
                title: title_at(row, 0)?,
                last_millis: row.get(2)?,
                entries: row.get(3)?,
                model,
                mode: row
                    .get::<Option<String>>(6)?
                    .map(super::decode::parsed)
                    .transpose()?,
                stopped,
            })
        },
    )?
    .ok_or_else(|| corrupt("session summary is missing"))
}

/// The session list's title.
pub(in crate::session) fn title(db: &Db) -> DbResult<Option<SessionTitle>> {
    let title = db.query_row(
        "SELECT text, source FROM session_title",
        Vec::new(),
        |row| title_at(row, 0),
    )?;
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

/// Why the root agent's turn stopped short of its answer, so resuming has something
/// to continue: its turn failed, or it was cut short while its conversation still
/// awaited the model or its latest request of any purpose had not completed.
/// `None` resumes idle. Each step is one indexed lookup, however long the journal.
pub(in crate::session) fn stopped_turn(db: &Db) -> DbResult<Option<TurnFailure>> {
    let latest = |kind: EntryKind| {
        let latest = db.query_row(
            "SELECT max(seq) FROM entry \
             WHERE agent = (SELECT id FROM agent WHERE parent IS NULL) AND kind = ?1",
            params![kind],
            |row| Ok(row.get::<Option<i64>>(0)?),
        );
        latest.map(Option::flatten)
    };
    let message = latest(EntryKind::MessageCommitted)?;
    let failed = latest(EntryKind::AgentFailed)?;
    if let Some(failed) = failed.filter(|failed| message.is_none_or(|message| message < *failed)) {
        let failure = db.query_row(
            "SELECT failure, provider, detail FROM failure WHERE entry = ?1",
            params![failed],
            |row| failure_at(row, 0),
        )?;
        let failure = failure.ok_or_else(|| corrupt("an agent failure has no detail"))?;
        return Ok(Some(TurnFailure::Failed(failure)));
    }
    if let Some(message) = message {
        let awaiting = db.query_row(
            "SELECT m.role <> ?2 OR EXISTS (SELECT 1 FROM assistant_item i \
               JOIN tool_call c ON c.item = i.id WHERE i.message = m.id) \
             FROM message_commit mc JOIN message m ON m.id = mc.message WHERE mc.entry = ?1",
            params![message, MessageRole::Assistant],
            |row| Ok(row.get::<i64>(0)? != 0),
        )?;
        if awaiting.ok_or_else(|| corrupt("a committed message is missing"))? {
            return Ok(Some(TurnFailure::Interrupted));
        }
    }
    let Some(request) = latest(EntryKind::ModelRequested)? else {
        return Ok(None);
    };
    let completed = db.query_row(
        "SELECT EXISTS (SELECT 1 FROM model_attempt a JOIN attempt_outcome o ON o.attempt = a.entry \
         WHERE a.request = ?1 AND o.kind IN (?2, ?3))",
        params![request, EntryKind::ResponseCompleted, EntryKind::Compaction],
        |row| Ok(row.get::<i64>(0)? != 0),
    )?;
    Ok((completed == Some(false)).then_some(TurnFailure::Interrupted))
}

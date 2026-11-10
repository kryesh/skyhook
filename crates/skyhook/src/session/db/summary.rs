//! The journal's counters, and the session list's summary and title.

use std::sync::LazyLock;

use libsql::Row;

use super::agent_state::last_at;
use super::{Db, DbResult, corrupt, decode::*, enum_column, optional_enum_column, params};
use crate::{
    session::TurnFailure,
    session::{
        EntryKind, MessageRole, TitleSource, UserPartKind, agent_state::stopped, core::Counters,
        requests::PhaseKind,
    },
};

/// A session list row, read without decoding the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSummary {
    /// The host that created this session.
    pub host: crate::identity::HostName,
    /// The user's title, else the newest automatic one, else the first text the
    /// user sent the root agent, as a `Prompt` title.
    pub title: Option<SessionTitle>,
    /// When the session last did something: its newest record that
    /// [`is_activity`](crate::session::SessionEvent::is_activity).
    pub last_millis: i64,
    /// How many entries the journal holds.
    pub entries: u64,
    /// The root agent's applied mode.
    pub mode: Option<crate::config::mode::ModeName>,
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
    let host = host(db)?;
    let root = db.query_row(
        "SELECT (SELECT mode_name FROM agent_mode WHERE entry = s.granted), \
           s.last, f.failure, f.provider, f.detail, \
           (SELECT phase FROM request_state WHERE request = s.request) \
         FROM agent_state s JOIN agent a ON a.id = s.agent AND a.parent IS NULL \
         LEFT JOIN failure f ON f.entry = s.last_entry",
        Vec::new(),
        |row| {
            let mode = row.get::<Option<String>>(0)?.map(parsed).transpose()?;
            let last = last_at(row, 1)?;
            let phase = optional_enum_column::<PhaseKind>(row, 5)?;
            let incomplete = phase.is_some_and(|phase| phase != PhaseKind::Completed);
            Ok((mode, stopped(last.as_ref(), incomplete)))
        },
    )?;
    let (mode, stopped) = root.unwrap_or_default();
    db.query_row(
        format!(
            "SELECT t.text, t.source, coalesce(({LAST_ACTIVITY}), 0), \
               (SELECT coalesce(max(seq), 0) FROM entry) FROM (SELECT 1) LEFT JOIN ({}) t ON true",
            *TITLE
        ),
        title_params(),
        |row| {
            Ok(SessionSummary {
                host: host.clone(),
                title: title_at(row, 0)?,
                last_millis: row.get(2)?,
                entries: row.get(3)?,
                mode: mode.clone(),
                stopped: stopped.clone(),
            })
        },
    )?
    .ok_or_else(|| corrupt("session summary is missing"))
}

/// The immutable session origin, read independently of the runtime's host.
pub(in crate::session) fn host(db: &Db) -> DbResult<crate::identity::HostName> {
    db.query_row("SELECT host FROM session", Vec::new(), |row| {
        parsed(row.get(0)?)
    })?
    .ok_or_else(|| corrupt("session origin is missing"))
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

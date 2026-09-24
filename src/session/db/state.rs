//! Current-state queries over the schema's views.

use std::collections::HashMap;

use super::{Db, DbResult, corrupt};
use crate::identity::{AgentId, SessionId};
use crate::session::RequestSeq;

/// A session list row, read without decoding the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSummary {
    pub title: Option<String>,
    /// The first text the user sent the root agent.
    pub preview: Option<String>,
    pub last_millis: i64,
    pub entries: u64,
    /// The root agent's applied model profile.
    pub model: Option<String>,
    /// The root agent's applied mode.
    pub mode: Option<String>,
}

pub(in crate::session) fn summary(db: &Db) -> DbResult<SessionSummary> {
    db.query_row(
        "SELECT title, preview, coalesce(last_millis, 0), entries, model, mode FROM session_summary",
        Vec::new(),
        |row| {
            Ok(SessionSummary {
                title: row.get(0)?,
                preview: row.get(1)?,
                last_millis: row.get(2)?,
                entries: row.get(3)?,
                model: row.get(4)?,
                mode: row.get(5)?,
            })
        },
    )?
    .ok_or_else(|| corrupt("session summary is missing"))
}

/// Work a stopped process left open, to be settled before a session resumes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct InterruptedWork {
    /// Agent, request sequence and attempt number of attempts without an outcome.
    pub attempts: Vec<(AgentId, RequestSeq, u64)>,
    /// Agent, call id and tool name of committed calls without a result.
    pub calls: Vec<(AgentId, String, String)>,
}

pub(in crate::session) fn interrupted_work(
    db: &Db,
    session: SessionId,
) -> DbResult<InterruptedWork> {
    let agents = agent_paths(db, session)?;
    let agent = |id: i64| {
        agents
            .get(&id)
            .cloned()
            .ok_or_else(|| corrupt("agent path is missing"))
    };
    let attempts = db.query(
        "SELECT e.agent, a.request, a.attempt FROM open_attempt o \
         JOIN model_attempt a ON a.entry = o.attempt JOIN entry e ON e.seq = a.entry \
         ORDER BY a.entry",
        Vec::new(),
        |row| {
            Ok((
                agent(row.get(0)?)?,
                super::decode::sequence(row.get(1)?).request(),
                row.get(2)?,
            ))
        },
    )?;
    let calls = db.query(
        "SELECT e.agent, c.call_id, c.name FROM unanswered_call u \
         JOIN tool_call c ON c.item = u.call \
         JOIN assistant_item i ON i.id = c.item \
         JOIN message_commit m ON m.message = u.message JOIN entry e ON e.seq = m.entry \
         ORDER BY m.entry, i.position",
        Vec::new(),
        |row| Ok((agent(row.get(0)?)?, row.get(1)?, row.get(2)?)),
    )?;
    Ok(InterruptedWork { attempts, calls })
}

/// Agent identities by row; parents always precede their children.
pub(in crate::session) fn agent_paths(
    db: &Db,
    session: SessionId,
) -> DbResult<HashMap<i64, AgentId>> {
    let mut agents = HashMap::new();
    let rows = db.query(
        "SELECT id, parent, child_index FROM agent ORDER BY id",
        Vec::new(),
        |row| {
            Ok((
                row.get::<i64>(0)?,
                row.get::<Option<i64>>(1)?,
                row.get::<Option<u32>>(2)?,
            ))
        },
    )?;
    for (id, parent, index) in rows {
        let agent = match (parent, index) {
            (None, None) => AgentId::root(session),
            (Some(parent), Some(index)) => agents
                .get(&parent)
                .map(|parent: &AgentId| parent.child(index))
                .ok_or_else(|| corrupt("agent parent is missing"))?,
            _ => return Err(corrupt("agent path is inconsistent")),
        };
        agents.insert(id, agent);
    }
    Ok(agents)
}

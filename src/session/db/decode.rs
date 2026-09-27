//! Reassemble session events from normalized rows. Each schema section's module
//! loads the events of its own tables.

use std::collections::HashMap;

use serde::de::DeserializeOwned;

use super::{message::Messages, *};
use crate::{
    execution::{ExecutionLocation, path_from_bytes},
    identity::{AgentId, EventId, JobId, SessionId},
    provider::profile::ModelRef,
    session::{EntryKind, EventRecord, RecordSeq, SessionEvent},
    tool::policy::Capability,
};

/// The decoded events of entries with subtype rows, by entry.
pub(super) struct Events<'a> {
    pub db: &'a Db,
    by_entry: HashMap<i64, SessionEvent>,
}

impl Events<'_> {
    /// Add one event per row of `sql`, keyed by the entry in its first column.
    pub(super) fn load(
        &mut self,
        sql: &str,
        event: impl FnMut(&Row) -> DbResult<SessionEvent>,
    ) -> DbResult<()> {
        self.load_with(sql, Vec::new(), event)
    }

    pub(super) fn load_with(
        &mut self,
        sql: &str,
        params: Vec<Value>,
        mut event: impl FnMut(&Row) -> DbResult<SessionEvent>,
    ) -> DbResult<()> {
        let rows = self
            .db
            .query(sql, params, |row| Ok((row.get::<i64>(0)?, event(row)?)))?;
        self.by_entry.extend(rows);
        Ok(())
    }
}

pub(super) fn parse_json<T: DeserializeOwned>(text: &str) -> DbResult<T> {
    serde_json::from_str(text).map_err(|error| corrupt(error.to_string()))
}

/// A newtype validated when it was written (an item or block id, scope, job or
/// target name) from its column; anything else is corruption.
pub(super) fn parsed<T>(text: String) -> DbResult<T>
where
    T: TryFrom<String>,
    T::Error: std::fmt::Display,
{
    T::try_from(text).map_err(|error| corrupt(error.to_string()))
}

pub(super) fn bytes<const N: usize>(value: Vec<u8>) -> DbResult<[u8; N]> {
    value
        .try_into()
        .map_err(|_| corrupt("identifier has the wrong length"))
}

/// A journal sequence read back from its row.
pub(super) fn sequence(value: i64) -> RecordSeq {
    RecordSeq::new(u64_of(value))
}

pub(in crate::session) fn u64_of(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

pub(super) fn job(value: i64) -> DbResult<JobId> {
    JobId::new(u64_of(value)).map_err(|error| corrupt(error.to_string()))
}

/// Index rows of one query by their first column.
pub(super) fn keyed<T>(
    db: &Db,
    sql: &str,
    mut map: impl FnMut(&Row) -> DbResult<T>,
) -> DbResult<HashMap<i64, T>> {
    Ok(db
        .query(sql, Vec::new(), |row| Ok((row.get::<i64>(0)?, map(row)?)))?
        .into_iter()
        .collect())
}

/// Group rows of one query (ordered by position) under their first column.
pub(super) fn grouped<T>(
    db: &Db,
    sql: &str,
    mut map: impl FnMut(&Row) -> DbResult<T>,
) -> DbResult<HashMap<i64, Vec<T>>> {
    let mut groups: HashMap<i64, Vec<T>> = HashMap::new();
    for (key, value) in db.query(sql, Vec::new(), |row| Ok((row.get::<i64>(0)?, map(row)?)))? {
        groups.entry(key).or_default().push(value);
    }
    Ok(groups)
}

/// Capability sets iterate in declaration order.
pub(super) fn sorted(mut capabilities: Vec<Capability>) -> Vec<Capability> {
    capabilities.sort();
    capabilities
}

/// A profile's qualified name, validated when it was written.
pub(super) fn model_ref(provider: &str, name: &str) -> DbResult<ModelRef> {
    let invalid =
        |error: crate::provider::profile::NameError| corrupt(format!("model_profile: {error}"));
    Ok(ModelRef::new(
        provider.parse().map_err(invalid)?,
        name.parse().map_err(invalid)?,
    ))
}

pub(super) fn location(target: String, workspace: Vec<u8>) -> DbResult<ExecutionLocation> {
    Ok(ExecutionLocation {
        target: parsed(target)?,
        workspace: path_from_bytes(workspace),
    })
}

/// Decode every committed record in sequence order.
pub(in crate::session) fn decode_records(
    db: &Db,
    session: SessionId,
) -> DbResult<Vec<EventRecord>> {
    let public_id = db.query_row("SELECT public_id FROM session", Vec::new(), |row| {
        Ok(row.get::<Vec<u8>>(0)?)
    })?;
    if public_id.is_some_and(|id| id.as_slice() != session.to_bytes()) {
        return Err(corrupt("database belongs to another session"));
    }
    let messages = Messages::load(db)?;
    let todos = super::message::entry_todos(db)?;
    let mut events = Events {
        db,
        by_entry: HashMap::new(),
    };
    events.load("SELECT entry, kind, text FROM entry_text", |row| {
        let text = row.get(2)?;
        Ok(match enum_column(row, 1)? {
            EntryKind::TitleSet => SessionEvent::TitleSet { title: text },
            EntryKind::Status => SessionEvent::Status { message: text },
            kind => return Err(corrupt(format!("{kind} entry has a text row"))),
        })
    })?;
    contract::events(&mut events)?;
    messages.events(&mut events)?;
    request::events(&mut events, &messages, &todos)?;
    job::events(&mut events)?;
    approval::events(&mut events)?;
    let targets = contract::targets(db)?;
    let agents = agent_paths(db, session)?;
    let session_capabilities = sorted(db.query(
        "SELECT capability FROM session_capability",
        Vec::new(),
        |row| enum_column(row, 0),
    )?);
    let entries = db.query(
        "SELECT seq, public_id, agent, created_millis, kind FROM entry ORDER BY seq",
        Vec::new(),
        |row| {
            Ok((
                row.get::<i64>(0)?,
                row.get::<Vec<u8>>(1)?,
                row.get::<i64>(2)?,
                row.get::<i64>(3)?,
                enum_column::<EntryKind>(row, 4)?,
            ))
        },
    )?;
    let mut records = Vec::with_capacity(entries.len());
    for (seq, public_id, agent, created, kind) in entries {
        let event = match kind {
            EntryKind::SessionStarted => SessionEvent::SessionStarted {
                targets: targets.get(&seq).cloned().unwrap_or_default(),
                capabilities: session_capabilities.clone(),
            },
            EntryKind::TargetsUpserted => SessionEvent::TargetsUpserted {
                targets: targets.get(&seq).cloned().unwrap_or_default(),
            },
            EntryKind::TodosReplaced => SessionEvent::TodosReplaced {
                items: todos.get(&seq).cloned().unwrap_or_default(),
            },
            EntryKind::AgentCompleted => SessionEvent::AgentCompleted,
            EntryKind::AgentInterrupted => SessionEvent::AgentInterrupted,
            _ => events
                .by_entry
                .remove(&seq)
                .ok_or_else(|| corrupt(format!("entry {seq} ({kind}) has no {kind} row")))?,
        };
        records.push(EventRecord {
            id: EventId::from_bytes(bytes(public_id)?),
            sequence: sequence(seq),
            timestamp_millis: created,
            agent: agents
                .get(&agent)
                .cloned()
                .ok_or_else(|| corrupt("entry agent is missing"))?,
            event,
        });
    }
    Ok(records)
}

/// Agent identities by row; parents always precede their children.
fn agent_paths(db: &Db, session: SessionId) -> DbResult<HashMap<i64, AgentId>> {
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

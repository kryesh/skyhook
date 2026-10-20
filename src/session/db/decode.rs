//! Reassemble session events from normalized rows. Each schema section's module
//! loads the events of its own tables.

use std::collections::HashMap;
use std::sync::Arc;

use serde::de::DeserializeOwned;

use super::{
    message::{Messages, Owner, Scope},
    *,
};
use crate::{
    execution::{ExecutionLocation, path_from_bytes},
    identity::{AgentId, EventId, JobId, SessionId},
    provider::profile::ModelRef,
    session::{EntryKind, EventRecord, RecordSeq, SessionEvent},
    tool::policy::Capability,
};

/// The entries a decode reassembles: every one, or those listed.
pub(in crate::session) type Entries<'a> = Scope<'a, RecordSeq>;

/// The decoded events of entries with subtype rows, by entry.
pub(super) struct Events<'a> {
    pub db: &'a Db,
    /// The entries whose rows each load keeps.
    pub scope: Entries<'a>,
    by_entry: HashMap<i64, SessionEvent>,
}

impl Events<'_> {
    /// `sql` with `{scope}` replaced by a filter keeping the rows whose `column` names
    /// an entry in scope.
    pub(super) fn scoped(&self, sql: &str, column: &'static str) -> DbResult<Query> {
        self.scope.query(sql, Owner::Message(column))
    }

    /// Add one event per row of `sql`, keyed by the entry in its first column, which
    /// `{scope}` filters as `column`.
    pub(super) fn load(
        &mut self,
        sql: &str,
        column: &'static str,
        mut event: impl FnMut(&Row) -> DbResult<SessionEvent>,
    ) -> DbResult<()> {
        let Query { sql, params } = self.scoped(sql, column)?;
        let rows = (self.db).query(sql, params, |row| Ok((row.get::<i64>(0)?, event(row)?)))?;
        rows.into_iter()
            .try_for_each(|(entry, event)| self.insert(entry, event))
    }

    /// File `event` under `entry`, which holds at most one.
    pub(super) fn insert(&mut self, entry: i64, event: SessionEvent) -> DbResult<()> {
        match self.by_entry.insert(entry, event) {
            Some(_) => Err(corrupt(format!("entry {entry} has rows of two kinds"))),
            None => Ok(()),
        }
    }
}

/// The rows `key` owns; rows no owner takes are corrupt.
pub(super) fn take<T: Default>(rows: &mut HashMap<i64, T>, key: i64) -> T {
    rows.remove(&key).unwrap_or_default()
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

/// The entry a row selects first.
pub(super) fn entry(row: &Row) -> DbResult<RecordSeq> {
    Ok(sequence(row.get(0)?))
}

pub(in crate::session) fn u64_of(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

pub(super) fn job(value: i64) -> DbResult<JobId> {
    JobId::new(u64_of(value)).map_err(|error| corrupt(error.to_string()))
}

/// A statement and its parameters; plain text takes none.
pub(super) struct Query {
    pub sql: String,
    pub params: Vec<Value>,
}

impl From<&str> for Query {
    fn from(sql: &str) -> Self {
        Self {
            sql: sql.to_owned(),
            params: Vec::new(),
        }
    }
}

/// Index rows of one query by their first column.
pub(super) fn keyed<T>(
    db: &Db,
    query: impl Into<Query>,
    mut map: impl FnMut(&Row) -> DbResult<T>,
) -> DbResult<HashMap<i64, T>> {
    let Query { sql, params } = query.into();
    Ok(db
        .query(sql, params, |row| Ok((row.get::<i64>(0)?, map(row)?)))?
        .into_iter()
        .collect())
}

/// Group rows of one query (ordered by position) under their first column.
pub(super) fn grouped<T>(
    db: &Db,
    query: impl Into<Query>,
    mut map: impl FnMut(&Row) -> DbResult<T>,
) -> DbResult<HashMap<i64, Vec<T>>> {
    let Query { sql, params } = query.into();
    let mut groups: HashMap<i64, Vec<T>> = HashMap::new();
    for (key, value) in db.query(sql, params, |row| Ok((row.get::<i64>(0)?, map(row)?)))? {
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

/// Decode the committed records `scope` selects, in sequence order.
pub(in crate::session) fn decode_records(
    db: &Db,
    session: SessionId,
    scope: Entries,
) -> DbResult<Vec<Arc<EventRecord>>> {
    // Every table is read for any scope, so an empty one would cost a full decode.
    if let Scope::Only([]) = scope {
        return Ok(Vec::new());
    }
    let public_id = db.query_row("SELECT public_id FROM session", Vec::new(), |row| {
        Ok(row.get::<Vec<u8>>(0)?)
    })?;
    if public_id.is_some_and(|id| id.as_slice() != session.to_bytes()) {
        return Err(corrupt("database belongs to another session"));
    }
    let mut events = Events {
        db,
        scope,
        by_entry: HashMap::new(),
    };
    let messages = match scope {
        Scope::All => Messages::load(db, Scope::All)?,
        Scope::Only(_) => {
            let referenced = events.scoped(
                "SELECT message FROM message_commit {scope} \
                 UNION SELECT message FROM compaction {scope} \
                 UNION SELECT t.message FROM model_request_tail t \
                 WHERE t.request IN (SELECT entry FROM model_request {scope})",
                "entry",
            )?;
            let ids = db.query(referenced.sql, referenced.params, |row| {
                Ok(row.get::<i64>(0)?)
            })?;
            Messages::load(db, Scope::Only(&ids))?
        }
    };
    let mut todos = super::message::entry_todos(&events)?;
    events.load(
        "SELECT entry, text FROM entry_text {scope}",
        "entry",
        |row| {
            let message = row.get(1)?;
            Ok(SessionEvent::Status { message })
        },
    )?;
    events.load(
        "SELECT entry, source, text FROM title {scope}",
        "entry",
        |row| {
            let source = enum_column(row, 1)?;
            let title = row.get(2)?;
            Ok(SessionEvent::TitleSet { title, source })
        },
    )?;
    contract::events(&mut events)?;
    messages.events(&mut events)?;
    request::events(&mut events, &messages, &mut todos)?;
    job::events(&mut events)?;
    let mut targets = contract::targets(&events)?;
    let agents = agent_paths(db, session, scope)?;
    let Query { sql, params } = events.scoped(
        "SELECT seq, public_id, agent, created_millis, kind FROM entry {scope} ORDER BY seq",
        "seq",
    )?;
    let entries = db.query(sql, params, |row| {
        Ok((
            row.get::<i64>(0)?,
            row.get::<Vec<u8>>(1)?,
            row.get::<i64>(2)?,
            row.get::<i64>(3)?,
            enum_column::<EntryKind>(row, 4)?,
        ))
    })?;
    let mut records = Vec::with_capacity(entries.len());
    for (seq, public_id, agent, created, kind) in entries {
        // Rows filed under an entry of another kind, or missing, are corrupt.
        let event = match (kind, events.by_entry.remove(&seq)) {
            (EntryKind::SessionStarted, None) => SessionEvent::SessionStarted {
                targets: take(&mut targets, seq),
            },
            (EntryKind::TargetsUpserted, None) => SessionEvent::TargetsUpserted {
                targets: take(&mut targets, seq),
            },
            (EntryKind::TodosReplaced, None) => SessionEvent::TodosReplaced {
                items: take(&mut todos, seq),
            },
            (EntryKind::AgentCompleted, None) => SessionEvent::AgentCompleted,
            (EntryKind::AgentInterrupted, None) => SessionEvent::AgentInterrupted,
            (EntryKind::TitleCleared, None) => SessionEvent::TitleCleared,
            (_, Some(event)) if event.kind() == kind => event,
            _ => {
                return Err(corrupt(format!(
                    "entry {seq} ({kind}) does not match its rows"
                )));
            }
        };
        records.push(Arc::new(EventRecord {
            id: EventId::from_bytes(bytes(public_id)?),
            sequence: sequence(seq),
            timestamp_millis: created,
            agent: agents
                .get(&agent)
                .cloned()
                .ok_or_else(|| corrupt("entry agent is missing"))?,
            event,
        }));
    }
    if !targets.is_empty() || !todos.is_empty() {
        return Err(corrupt(
            "target or todo rows belong to another kind's entry",
        ));
    }
    Ok(records)
}

/// The identities of the agents `scope`'s entries name, with their ancestors, by
/// row; parents always precede their children.
pub(in crate::session) fn agent_paths(
    db: &Db,
    session: SessionId,
    scope: Entries,
) -> DbResult<HashMap<i64, AgentId>> {
    let mut agents = HashMap::new();
    let Query { sql, params } = match scope {
        Scope::All => Query::from("SELECT id, parent, child_index FROM agent ORDER BY id"),
        Scope::Only(_) => scope.query(
            "WITH RECURSIVE named(id) AS (SELECT agent FROM entry {scope} \
               UNION SELECT a.parent FROM agent a JOIN named ON a.id = named.id \
               WHERE a.parent IS NOT NULL) \
             SELECT a.id, a.parent, a.child_index \
             FROM named CROSS JOIN agent a ON a.id = named.id ORDER BY a.id",
            Owner::Message("seq"),
        )?,
    };
    let rows = db.query(sql, params, |row| {
        Ok((
            row.get::<i64>(0)?,
            row.get::<Option<i64>>(1)?,
            row.get::<Option<u32>>(2)?,
        ))
    })?;
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

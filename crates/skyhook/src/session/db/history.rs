//! One agent's history, read by index: its projection, its completed requests, and the
//! entries a record references.

use super::{
    decode::*,
    encode::json,
    message::{MessageRole, Messages, Scope},
    request::{PhaseKind, reported_at},
    *,
};
use crate::{
    identity::SessionId,
    session::{
        CompletedRequest, EntryKind, Message, ModelPurpose, Projected, Projection, Referenced,
    },
};

pub(in crate::session) use super::message::committed;

/// The checkpoint a projection opens with.
#[derive(Clone, Copy)]
pub(in crate::session) enum Opening {
    /// The agent's latest within the bound.
    Latest,
    /// The one a request named, if any.
    Named(Option<RecordSeq>),
}

/// `agent`'s latest entry of `kind` at or before `through`.
fn latest(db: &Db, agent: i64, kind: EntryKind, through: RecordSeq) -> DbResult<Option<RecordSeq>> {
    let found = db.query_row(
        "SELECT max(seq) FROM entry WHERE agent = ?1 AND kind = ?2 AND seq <= ?3",
        params![agent, kind, through],
        |row| Ok(row.get::<Option<i64>>(0)?.map(sequence)),
    )?;
    Ok(found.flatten())
}

/// `agent`'s history through `through`: the checkpoint `opening` names with its
/// retained sources, then the history fold of the agent's records after its frontier,
/// from the request the first response after it may answer.
pub(in crate::session) fn project(
    db: &Db,
    session: SessionId,
    agent: i64,
    opening: Opening,
    through: RecordSeq,
) -> DbResult<Projection> {
    let checkpoint = match opening {
        Opening::Latest => latest(db, agent, EntryKind::Compaction, through)?,
        Opening::Named(checkpoint) => checkpoint,
    };
    let mut projection = Projection::default();
    let mut frontier = RecordSeq::default();
    if let Some(entry) = checkpoint {
        let (at, summary, retained) =
            compaction(db, entry)?.ok_or_else(|| corrupt("checkpoint is not a compaction"))?;
        frontier = at;
        let summary = Messages::load(db, Scope::Only(&[summary]))?.message(summary)?;
        projection.checkpoint = Some(entry);
        projection.messages.push(Projected::unbound(entry, summary));
        let mut sources = committed(db, &retained)?;
        for source in retained {
            let message = (sources.remove(&source))
                .ok_or_else(|| corrupt("retained source is not a committed message"))?;
            (projection.messages).push(Projected::unbound(source.into(), message));
        }
    }
    let kinds: Vec<_> = (EntryKind::ALL.into_iter())
        .filter(|kind| kind.history_folded())
        .collect();
    let mut entries = db.query(
        "SELECT seq FROM entry WHERE agent = ?1 AND seq > ?2 AND seq <= ?3 \
         AND kind IN (SELECT value FROM json_each(?4))",
        params![agent, frontier, through, json(&kinds)?],
        entry,
    )?;
    entries.extend(latest(db, agent, EntryKind::ModelRequested, frontier)?);
    entries.sort_unstable();
    let mut requested = None;
    for record in decode_records(db, session, Scope::Only(&entries))? {
        projection.apply(&mut requested, &record);
    }
    Ok(projection)
}

/// The checkpoint journaled at `entry`, if one was: its frontier, its summary's message
/// row and its retained sources, in order.
fn compaction(db: &Db, entry: RecordSeq) -> DbResult<Option<(RecordSeq, i64, Vec<MessageSeq>)>> {
    let Some((frontier, summary)) = db.query_row(
        "SELECT frontier, message FROM compaction WHERE entry = ?1",
        params![entry],
        |row| Ok((sequence(row.get(0)?), row.get::<i64>(1)?)),
    )?
    else {
        return Ok(None);
    };
    let retained = db.query(
        "SELECT source FROM compaction_retained WHERE compaction = ?1 ORDER BY source",
        params![entry],
        |row| Ok(sequence(row.get(0)?).message()),
    )?;
    Ok(Some((frontier, summary, retained)))
}

/// `agent`'s requests after `after` that completed by `through`, in order.
pub(in crate::session) fn completed_requests(
    db: &Db,
    agent: i64,
    after: RecordSeq,
    through: RecordSeq,
) -> DbResult<Vec<CompletedRequest>> {
    db.query(
        "SELECT r.context, r.history_through, s.reported, s.reported_input_tokens, \
           s.reported_cached_input_tokens, s.reported_cache_write_input_tokens, \
           s.reported_output_tokens \
         FROM entry e JOIN model_request r ON r.entry = e.seq \
         JOIN request_state s ON s.request = e.seq \
         WHERE e.agent = ?1 AND e.kind = ?2 AND e.seq > ?3 AND s.phase = ?4 AND s.phased <= ?5 \
         ORDER BY e.seq",
        params![
            agent,
            EntryKind::ModelRequested,
            after,
            PhaseKind::Completed,
            through
        ],
        |row| {
            Ok(CompletedRequest {
                context: sequence(row.get(0)?),
                through: (row.get::<Option<i64>>(1)?).map(|through| sequence(through).message()),
                usage: reported_at(row, 2)?,
            })
        },
    )
}

/// The commits of the agent of row `?1`, as `entry e` with their `message m`.
const COMMITS: &str = "FROM entry e JOIN message_commit c ON c.entry = e.seq \
     JOIN message m ON m.id = c.message WHERE e.agent = ?1 AND e.kind = ?2";

/// The exchange `agent`'s commit `origin` opens through `through`: the commit and its
/// tool results. None unless `origin` is the agent's and not a state.
pub(in crate::session) fn exchange(
    db: &Db,
    agent: i64,
    origin: MessageSeq,
    through: RecordSeq,
) -> DbResult<Option<Vec<(MessageSeq, Message)>>> {
    let role = db.query_row(
        format!("SELECT m.role {COMMITS} AND e.seq = ?3 AND e.seq <= ?4"),
        params![agent, EntryKind::MessageCommitted, origin, through],
        |row| enum_column::<MessageRole>(row, 0),
    )?;
    if role.is_none_or(|role| role == MessageRole::State) {
        return Ok(None);
    }
    let (results, _) = results(db, agent, origin.into(), through)?;
    let members: Vec<_> = std::iter::once(origin).chain(results).collect();
    let mut messages = committed(db, &members)?;
    (members.into_iter())
        .map(|member| {
            let message = (messages.remove(&member))
                .ok_or_else(|| corrupt("an exchange member is not a committed message"))?;
            Ok((member, message))
        })
        .collect::<DbResult<_>>()
        .map(Some)
}

/// The tool results `agent` committed after its commit `call` through `through`: the
/// run of them up to its next commit, which ends the run if there is one by then.
pub(in crate::session) fn results(
    db: &Db,
    agent: i64,
    call: RecordSeq,
    through: RecordSeq,
) -> DbResult<(Vec<MessageSeq>, Option<RecordSeq>)> {
    let kind = EntryKind::MessageCommitted;
    let end = db.query_row(
        format!(
            "SELECT e.seq {COMMITS} AND e.seq > ?3 AND e.seq <= ?4 AND m.role != ?5 \
             ORDER BY e.seq LIMIT 1"
        ),
        params![agent, kind, call, through, MessageRole::Tool],
        entry,
    )?;
    let results = db.query(
        "SELECT seq FROM entry WHERE agent = ?1 AND kind = ?2 AND seq > ?3 AND seq < ?4 \
         ORDER BY seq",
        params![agent, kind, call, end.unwrap_or(through.next())],
        |row| Ok(sequence(row.get(0)?).message()),
    )?;
    Ok((results, end))
}

/// `agent`'s latest commit before `before` that is not a tool result: the one that
/// opens the exchange of a result committed at `before`.
pub(in crate::session) fn opener(
    db: &Db,
    agent: i64,
    before: RecordSeq,
) -> DbResult<Option<RecordSeq>> {
    db.query_row(
        format!(
            "SELECT e.seq {COMMITS} AND e.seq < ?3 AND m.role != ?4 ORDER BY e.seq DESC LIMIT 1"
        ),
        params![
            agent,
            EntryKind::MessageCommitted,
            before,
            MessageRole::Tool
        ],
        entry,
    )
}

/// Whether `agent` replaced its todos, or compacted, after `after` and before `before`.
pub(in crate::session) fn todos_changed(
    db: &Db,
    agent: i64,
    after: RecordSeq,
    before: RecordSeq,
) -> DbResult<bool> {
    let changed = db.query_row(
        "SELECT EXISTS (SELECT 1 FROM entry WHERE agent = ?1 AND kind IN (?2, ?3) \
         AND seq > ?4 AND seq < ?5)",
        params![
            agent,
            EntryKind::TodosReplaced,
            EntryKind::Compaction,
            after,
            before
        ],
        |row| Ok(row.get::<bool>(0)?),
    )?;
    Ok(changed.unwrap_or_default())
}

/// Whether `agent` journaled a model context for `purpose` after `after` and before
/// `before`.
pub(in crate::session) fn context_replaced(
    db: &Db,
    agent: i64,
    purpose: ModelPurpose,
    after: RecordSeq,
    before: RecordSeq,
) -> DbResult<bool> {
    let replaced = db.query_row(
        "SELECT EXISTS (SELECT 1 FROM entry e JOIN model_context c ON c.entry = e.seq \
         WHERE e.agent = ?1 AND e.kind = ?2 AND e.seq > ?3 AND e.seq < ?4 AND c.purpose = ?5)",
        params![agent, EntryKind::ModelContext, after, before, purpose],
        |row| Ok(row.get::<bool>(0)?),
    )?;
    Ok(replaced.unwrap_or_default())
}

/// The ids of the calls `agent` committed before `before` and since its latest agent
/// request before it that no result answers yet, in commit order.
pub(in crate::session) fn unanswered(
    db: &Db,
    agent: i64,
    before: RecordSeq,
) -> DbResult<Vec<String>> {
    db.query(
        format!(
            "SELECT c.call_id FROM entry e JOIN message_commit m ON m.entry = e.seq \
             JOIN assistant_item i ON i.message = m.message JOIN tool_call c ON c.item = i.id \
             WHERE e.agent = ?4 AND e.kind = ?1 AND e.seq < ?5 AND e.seq > {} \
             AND NOT EXISTS (SELECT 1 FROM tool_result r WHERE r.call = c.item) \
             ORDER BY e.seq, i.position",
            since_agent_request("?4", "AND q.seq < ?5")
        ),
        params![
            EntryKind::MessageCommitted,
            EntryKind::ModelRequested,
            ModelPurpose::Agent,
            agent,
            before
        ],
        |row| Ok(row.get::<String>(0)?),
    )
}

/// The latest agent request of the agent the SQL `agent` names, within `bound`, or 0;
/// `?2` and `?3` bind the model_requested kind and the agent purpose.
pub(super) fn since_agent_request(agent: &str, bound: &str) -> String {
    format!(
        "coalesce((SELECT q.seq FROM entry q JOIN model_request r ON r.entry = q.seq \
           JOIN model_context x ON x.entry = r.context \
           WHERE q.agent = {agent} AND q.kind = ?2 AND x.purpose = ?3 {bound} \
           ORDER BY q.seq DESC LIMIT 1), 0)"
    )
}

/// What `agent`'s entry `entry` is, as a record's references need it; None unless the
/// agent journaled it.
pub(in crate::session) fn referenced(
    db: &Db,
    agent: i64,
    entry: RecordSeq,
) -> DbResult<Option<Referenced>> {
    let kind = db.query_row(
        "SELECT kind FROM entry WHERE seq = ?1 AND agent = ?2",
        params![entry, agent],
        |row| enum_column::<EntryKind>(row, 0),
    )?;
    let missing = || corrupt(format!("entry {entry} has no rows of its kind"));
    Ok(Some(match kind {
        None => return Ok(None),
        Some(EntryKind::ModelContext) => Referenced::Context(
            db.query_row(
                "SELECT purpose FROM model_context WHERE entry = ?1",
                params![entry],
                |row| enum_column(row, 0),
            )?
            .ok_or_else(missing)?,
        ),
        Some(EntryKind::ModelRequested) => Referenced::Request {
            context: db
                .query_row(
                    "SELECT context FROM model_request WHERE entry = ?1",
                    params![entry],
                    super::decode::entry,
                )?
                .ok_or_else(missing)?,
        },
        Some(EntryKind::Compaction) => {
            let (frontier, _, retained) = compaction(db, entry)?.ok_or_else(missing)?;
            Referenced::Checkpoint { frontier, retained }
        }
        Some(EntryKind::MessageCommitted) => Referenced::Commit,
        Some(_) => Referenced::Other,
    }))
}

/// The row of the agent that journaled `entry`.
pub(in crate::session) fn author(db: &Db, entry: RecordSeq) -> DbResult<Option<i64>> {
    db.query_row(
        "SELECT agent FROM entry WHERE seq = ?1",
        params![entry],
        |row| Ok(row.get::<i64>(0)?),
    )
}

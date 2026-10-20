//! A session's history a page at a time, read by index: an agent's transcript by
//! ordinal with what presenting it reads, the jobs of an owner or a parent, and
//! every agent's summary.

use std::{collections::HashMap, sync::Arc};

use super::{
    decode::*,
    encode::{agent_row, json},
    message::Scope,
    *,
};
use crate::{
    identity::{AgentId, JobId, SessionId},
    session::{
        AgentSummary, AgentsPage, Answer, EntryKind, EventRecord, Message, Page, PageFacts,
        RecordSeq, SessionEvent, TranscriptPage,
    },
};

/// How many entries `agent`'s transcript holds: the ordinal its next one takes.
pub(super) fn transcript_length(db: &Db, agent: i64) -> DbResult<u64> {
    let next = db.query_row(
        "SELECT coalesce(max(agent_ordinal) + 1, 0) FROM entry WHERE agent = ?1",
        params![agent],
        |row| Ok(u64_of(row.get(0)?)),
    )?;
    Ok(next.unwrap_or_default())
}

/// The queries of a page read, by [`Page`] variant in declaration order: `select`'s
/// rows that match `filter`, in `key` order, from either end or past `bound`, which
/// reads the page's key as `?1`. The limit is `?2`; `filter`'s parameters start at `?3`.
macro_rules! paged_sql {
    ($select:expr, $filter:literal, $key:literal, $bound:literal) => {
        [
            paged_sql!(@ $select, $filter, $key, ""),
            paged_sql!(@ $select, $filter, $key, "", " AND ", $key, " > ", $bound),
            paged_sql!(@ $select, $filter, $key, " DESC", " AND ", $key, " < ", $bound),
            paged_sql!(@ $select, $filter, $key, " DESC"),
        ]
    };
    (@ $select:expr, $filter:literal, $key:literal, $order:literal $(, $clause:expr)*) => {
        concat!($select, " WHERE ", $filter, $($clause,)* " ORDER BY ", $key, $order, " LIMIT ?2")
    };
}
pub(super) use paged_sql;

/// The rows `page` reads, in ascending order, by whichever [`paged_sql!`] query reads its
/// way: each binds the page's bound, the limit, then `params`.
pub(super) fn paged<K: Sql, T>(
    db: &Db,
    page: Page<K>,
    [first, after, before, last]: [&'static str; 4],
    params: Vec<Value>,
    limit: usize,
    map: impl FnMut(&Row) -> DbResult<T>,
) -> DbResult<Vec<T>> {
    let (sql, bound, descending) = match page {
        Page::First => (first, None, false),
        Page::After(key) => (after, Some(key), false),
        Page::Before(key) => (before, Some(key), true),
        Page::Last => (last, None, true),
    };
    let mut bound = params![bound, limit];
    bound.extend(params);
    let mut rows = db.query(sql, bound, map)?;
    if descending {
        rows.reverse();
    }
    Ok(rows)
}

/// Up to `limit` consecutive entries of `agent`'s transcript at `page`, by ordinal,
/// with the records presenting them reads.
pub(in crate::session) fn transcript(
    db: &Db,
    session: SessionId,
    agent: &AgentId,
    page: Page<u64>,
    limit: usize,
) -> DbResult<TranscriptPage> {
    let Some(row) = agent_row(db, agent)? else {
        return Ok(TranscriptPage {
            first: 0,
            records: Vec::new(),
            facts: PageFacts::default(),
        });
    };
    let entries = paged(
        db,
        page,
        paged_sql!(
            "SELECT seq, agent_ordinal FROM entry",
            "agent = ?3",
            "agent_ordinal",
            "?1"
        ),
        params![row],
        limit,
        |row| Ok((entry(row)?, u64_of(row.get(1)?))),
    )?;
    let first = match (entries.first(), page) {
        (Some((_, ordinal)), _) => *ordinal,
        (None, Page::After(ordinal)) => ordinal.saturating_add(1),
        // Nothing before a bound, or at all, starts at 0.
        (None, _) => 0,
    };
    let seqs: Vec<_> = entries.into_iter().map(|(seq, _)| seq).collect();
    let records = decode_records(db, session, Scope::Only(&seqs))?;
    let facts = facts(db, session, agent, &records)?;
    Ok(TranscriptPage {
        first,
        records,
        facts,
    })
}

/// What presenting `records`, entries of `agent`, reads beyond them.
fn facts(
    db: &Db,
    session: SessionId,
    agent: &AgentId,
    records: &[Arc<EventRecord>],
) -> DbResult<PageFacts> {
    let named = |kind: fn(&SessionEvent) -> bool| {
        let seqs: Vec<_> = (records.iter())
            .filter(|record| kind(&record.event))
            .map(|record| record.sequence)
            .collect();
        json(&seqs)
    };
    let assistant = named(|event| {
        matches!(
            event,
            SessionEvent::MessageCommitted {
                message: Message::Assistant(_)
            }
        )
    })?;
    let mut facts = PageFacts::default();
    let answers = db.query(
        "SELECT a.answer, a.request, t.created_millis FROM answer_turn a \
         LEFT JOIN entry t ON t.seq = a.turn WHERE a.answer IN (SELECT value FROM json_each(?1))",
        params![assistant.clone()],
        |row| {
            let request = sequence(row.get(1)?).request();
            let answer = Answer {
                request,
                turn: row.get(2)?,
            };
            Ok((entry(row)?.message(), answer))
        },
    )?;
    let mut requests: Vec<_> = (records.iter())
        .filter(|record| matches!(record.event, SessionEvent::ModelRequested { .. }))
        .map(|record| record.sequence)
        .collect();
    requests.extend(
        answers
            .iter()
            .map(|(_, answer)| RecordSeq::from(answer.request)),
    );
    facts.answers = answers.into_iter().collect();
    facts.requests = super::request::requests_at(db, agent, &requests)?;
    // A call's result and the job it started name it by its item.
    macro_rules! calls {
        ($join:literal) => {
            concat!(
                "SELECT mc.entry, c.call_id, ",
                $join,
                " WHERE mc.entry IN (SELECT value FROM json_each(?1))"
            )
        };
    }
    let results = db.query(
        calls!(
            "rc.entry FROM message_commit mc JOIN assistant_item i ON i.message = mc.message \
             JOIN tool_call c ON c.item = i.id JOIN tool_result r ON r.call = c.item \
             JOIN message_commit rc ON rc.message = r.message"
        ),
        params![assistant.clone()],
        |row| {
            Ok((
                (entry(row)?.message(), row.get::<String>(1)?),
                sequence(row.get(2)?),
            ))
        },
    )?;
    let seqs: Vec<_> = results.iter().map(|(_, result)| *result).collect();
    let decoded = decode_records(db, session, Scope::Only(&seqs))?;
    let decoded: HashMap<_, _> = (decoded.iter())
        .filter_map(|record| match &record.event {
            SessionEvent::MessageCommitted {
                message: Message::Tool(result),
            } => Some((record.sequence, result)),
            _ => None,
        })
        .collect();
    facts.results = (results.into_iter())
        .filter_map(|(call, result)| Some((call, (*decoded.get(&result)?).clone())))
        .collect();
    facts.admitted = (db.query(
        calls!(
            "j.id FROM message_commit mc JOIN assistant_item i ON i.message = mc.message \
             JOIN tool_call c ON c.item = i.id JOIN job j ON j.origin_call = c.item"
        ),
        params![assistant],
        |row| Ok((entry(row)?.message(), row.get::<String>(1)?)),
    )?)
    .into_iter()
    .collect();
    let failed = named(|event| matches!(event, SessionEvent::AgentFailed { .. }))?;
    facts.reported = (db.query(
        "SELECT entry FROM failed_turn WHERE entry IN (SELECT value FROM json_each(?1))",
        params![failed],
        entry,
    )?)
    .into_iter()
    .collect();
    Ok(facts)
}

/// Whether `agent` journaled a failed turn after `after`.
pub(in crate::session) fn failed_after(
    db: &Db,
    agent: &AgentId,
    after: RecordSeq,
) -> DbResult<bool> {
    let Some(row) = agent_row(db, agent)? else {
        return Ok(false);
    };
    let failed = db.query_row(
        "SELECT EXISTS (SELECT 1 FROM entry WHERE agent = ?1 AND kind = ?2 AND seq > ?3)",
        params![row, EntryKind::AgentFailed, after],
        |row| Ok(row.get::<i64>(0)? != 0),
    )?;
    Ok(failed == Some(true))
}

/// Up to `limit` of the jobs `owner` created that nest under no parent, at `page` in id
/// order.
pub(in crate::session) fn root_jobs(
    db: &Db,
    owner: &AgentId,
    page: Page<JobId>,
    limit: usize,
) -> DbResult<Vec<JobId>> {
    let Some(row) = agent_row(db, owner)? else {
        return Ok(Vec::new());
    };
    // Ids follow creation, so the creating entries' order is the jobs', and they are
    // contiguous, so a bound naming no job lies past every one. A job nests when a
    // script issued it: one with a parent that no model call launched.
    paged(
        db,
        page,
        paged_sql!(
            "SELECT j.id FROM entry e JOIN job j ON j.created = e.seq",
            "e.agent = ?3 AND e.kind = ?4 AND (j.parent IS NULL OR j.origin_call IS NOT NULL)",
            "e.seq",
            "coalesce((SELECT created FROM job WHERE id = ?1), 9223372036854775807)"
        ),
        params![row, EntryKind::JobCreated],
        limit,
        |row| job(row.get(0)?),
    )
}

/// Up to `limit` of `parent`'s children at `page`, in id order.
pub(in crate::session) fn child_jobs(
    db: &Db,
    parent: JobId,
    page: Page<JobId>,
    limit: usize,
) -> DbResult<Vec<JobId>> {
    paged(
        db,
        page,
        paged_sql!("SELECT id FROM job", "parent = ?3", "id", "?1"),
        params![parent],
        limit,
        |row| job(row.get(0)?),
    )
}

/// Every agent the session started, as [`AgentSummary`] describes it.
pub(in crate::session) fn agent_summaries(db: &Db, session: SessionId) -> DbResult<AgentsPage> {
    let through = counters(db)?.newest;
    let agents = agent_paths(db, session, Scope::All)?;
    let rows = db.query(
        "SELECT s.agent, e.seq, s.profiled, s.granted, s.todos, s.ended, a.owner_job, \
           t.created_millis, d.created_millis, b.created_millis \
         FROM agent_state s JOIN agent a ON a.id = s.agent \
         JOIN entry e ON e.agent = s.agent AND e.kind = ?1 \
         LEFT JOIN entry t ON t.seq = s.finished LEFT JOIN entry d ON d.seq = s.finished_entry \
         LEFT JOIN entry b ON b.seq = s.turn ORDER BY s.agent",
        params![EntryKind::AgentStarted],
        |row| {
            let mut records: Vec<_> = (1..=5)
                .filter_map(|at| row.get::<Option<i64>>(at).transpose())
                .map(|seq| Ok::<_, DbError>(sequence(seq?)))
                .collect::<DbResult<_>>()?;
            records.sort_unstable();
            records.dedup();
            let owner_job = row.get::<Option<i64>>(6)?.map(job).transpose()?;
            let since = row.get::<Option<i64>>(7)?;
            let finished = since.zip(row.get::<Option<i64>>(8)?);
            let agent = agents.get(&row.get(0)?).cloned();
            let agent = agent.ok_or_else(|| corrupt("an agent's state names no agent"))?;
            Ok((agent, records, owner_job, finished, row.get(9)?))
        },
    )?;
    let mut wanted: Vec<_> = rows
        .iter()
        .flat_map(|(_, records, ..)| records.clone())
        .collect();
    wanted.sort_unstable();
    wanted.dedup();
    let decoded = decode_records(db, session, Scope::Only(&wanted))?;
    let by_seq: std::collections::HashMap<_, _> = (decoded.iter())
        .map(|record| (record.sequence, record))
        .collect();
    let agents = (rows.into_iter())
        .map(|(agent, records, owner_job, finished, turn)| AgentSummary {
            agent,
            records: (records.iter())
                .filter_map(|seq| by_seq.get(seq).map(|record| Arc::clone(record)))
                .collect(),
            owner_job,
            finished,
            turn,
        })
        .collect();
    Ok(AgentsPage { through, agents })
}

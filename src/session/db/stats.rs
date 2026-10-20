//! What session statistics aggregate, read by index: each agent's start, model and
//! outcome as its state row names them, its tool calls, jobs and compactions counted in
//! SQL, and its requests a page at a time.

use std::collections::{BTreeMap, HashMap};

use super::{
    decode::*,
    state::{PROFILED, outcome_at},
    *,
};
use crate::{
    identity::{AgentId, JobId},
    job::JobRole,
    provider::profile::ModelRef,
    session::{
        EntryKind, RequestRecord, RequestSeq,
        stats::{AgentOutcome, JobCounts, ToolStats},
    },
};

/// What an agent's start records, the model it last applied, its owner job's name, and
/// how its work last ended since its latest request, with when.
pub(in crate::session) struct Started {
    pub started_millis: i64,
    pub model: ModelRef,
    pub owner_job: Option<JobId>,
    pub name: Option<String>,
    pub outcome: AgentOutcome,
}

/// Every agent's start, keyed by agent row, as its state row names it.
pub(in crate::session) fn starts(db: &Db) -> DbResult<HashMap<i64, Started>> {
    let rows = db.query(
        format!(
            "SELECT a.id, e.created_millis, p.provider, p.name, a.owner_job, j.name, \
               s.outcome, f.failure, f.provider, f.detail, x.created_millis \
             FROM agent_state s JOIN agent a ON a.id = s.agent \
             JOIN entry e ON e.agent = a.id AND e.kind = ?1 \
             JOIN model_profile p ON p.id = {PROFILED} LEFT JOIN job j ON j.id = a.owner_job \
             LEFT JOIN entry x ON x.seq = s.outcome_entry \
             LEFT JOIN failure f ON f.entry = s.outcome_entry"
        ),
        params![EntryKind::AgentStarted],
        |row| {
            let owner_job = row.get::<Option<i64>>(4)?.map(job).transpose()?;
            let outcome = outcome_at(row, 6)?;
            let outcome =
                outcome.map(|outcome| Ok::<_, DbError>(AgentOutcome::of(outcome, row.get(10)?)));
            Ok((
                row.get::<i64>(0)?,
                Started {
                    started_millis: row.get(1)?,
                    model: model_ref(&row.get::<String>(2)?, &row.get::<String>(3)?)?,
                    owner_job,
                    name: row.get(5)?,
                    outcome: outcome.transpose()?.unwrap_or_default(),
                },
            ))
        },
    )?;
    Ok(rows.into_iter().collect())
}

/// The tool calls of the agent at `row` by tool name: how many, how many results were
/// errors, and how many no result answers.
pub(in crate::session) fn calls(db: &Db, row: i64) -> DbResult<BTreeMap<String, ToolStats>> {
    let calls = db.query(
        "SELECT c.name, count(*), coalesce(sum(r.is_error), 0), count(*) - count(r.message) \
         FROM entry e JOIN message_commit mc ON mc.entry = e.seq \
         JOIN assistant_item i ON i.message = mc.message JOIN tool_call c ON c.item = i.id \
         LEFT JOIN tool_result r ON r.call = c.item \
         WHERE e.agent = ?1 AND e.kind = ?2 GROUP BY c.name",
        params![row, EntryKind::MessageCommitted],
        |row| {
            let calls = ToolStats {
                calls: u64_of(row.get(1)?),
                errors: u64_of(row.get(2)?),
                unanswered: u64_of(row.get(3)?),
            };
            Ok((row.get(0)?, calls))
        },
    )?;
    Ok(calls.into_iter().collect())
}

/// How many jobs of each role the agent at `row` created.
pub(in crate::session) fn jobs(db: &Db, row: i64) -> DbResult<JobCounts> {
    let roles = db.query(
        "SELECT j.role, count(*) FROM entry e JOIN job j ON j.created = e.seq \
         WHERE e.agent = ?1 AND e.kind = ?2 GROUP BY j.role",
        params![row, EntryKind::JobCreated],
        |row| Ok((enum_column(row, 0)?, u64_of(row.get(1)?))),
    )?;
    let mut counts = JobCounts::default();
    for (role, count) in roles {
        *match role {
            JobRole::Tool | JobRole::Wait => &mut counts.tools,
            JobRole::Agent => &mut counts.agents,
            JobRole::Script => &mut counts.scripts,
            JobRole::Question => &mut counts.questions,
        } += count;
    }
    Ok(counts)
}

/// How many entries of `kind` the agent at `row` journaled.
pub(in crate::session) fn count(db: &Db, row: i64, kind: EntryKind) -> DbResult<u64> {
    let count = db.query_row(
        "SELECT count(*) FROM entry WHERE agent = ?1 AND kind = ?2",
        params![row, kind],
        |row| Ok(u64_of(row.get(0)?)),
    )?;
    Ok(count.unwrap_or_default())
}

/// When the first entry was journaled, and the latest time any entry was dated.
pub(in crate::session) fn span(db: &Db) -> DbResult<(i64, i64)> {
    let span = db.query_row(
        "SELECT coalesce((SELECT created_millis FROM entry ORDER BY seq LIMIT 1), 0), \
           coalesce(max(created_millis), 0) FROM entry",
        Vec::new(),
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(span.unwrap_or_default())
}

/// Every request of `agent`, a page at a time, as [`super::requests`] reads them.
pub(in crate::session) fn each_request(
    db: &Db,
    agent: &AgentId,
    mut each: impl FnMut(RequestSeq, RequestRecord),
) -> DbResult<()> {
    const PAGE: usize = 256;
    let mut page = crate::session::Page::First;
    loop {
        let requests = super::requests(db, agent, page, PAGE)?;
        let full = requests.len() == PAGE;
        let Some((last, _)) = requests.last() else {
            return Ok(());
        };
        page = crate::session::Page::After(*last);
        requests
            .into_iter()
            .for_each(|(seq, request)| each(seq, request));
        // A short page is the last.
        if !full {
            return Ok(());
        }
    }
}

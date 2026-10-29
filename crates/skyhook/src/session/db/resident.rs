//! What opening a session reads: every agent's state row with its newest request and
//! model contexts, the calls no result answers yet, the resident jobs' status rows, and
//! the records the root's contract, each agent's todos, the latest mode editions and the
//! targets' latest revisions are journaled in. Nothing is folded from history.

use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use super::{
    decode::*,
    encode::json,
    history::since_agent_request,
    job::{JobRow, job_rows},
    message::Scope,
    request::{Named, named_requests},
    state::agent_states,
    *,
};
use crate::{
    agent::todo::{self, AgentTodos},
    identity::{AgentId, JobId, SessionId},
    job::JobSeeds,
    session::{
        EntryKind, EventRecord, MessageSeq, ModelPurpose, RecordSeq, RequestRecord, RequestSeq,
        ResidentLedger, agent_state::AgentState,
    },
};

/// What a session opens to, read from its rows.
pub(in crate::session) struct Opening {
    pub ledger: ResidentLedger,
    pub agents: HashMap<AgentId, AgentState>,
    /// The records the root's contract, the latest mode editions, each agent's todos
    /// and each target's latest revision are journaled in, in sequence order, and the
    /// root's highest child index.
    pub records: Vec<Arc<EventRecord>>,
    pub children: u32,
    pub todos: BTreeMap<AgentId, AgentTodos>,
    /// Each agent's request its context gauge shows.
    pub gauged: Vec<(RequestSeq, RequestRecord)>,
    /// The resident jobs, with their ancestors.
    pub jobs: Vec<JobRow>,
    pub seeds: JobSeeds,
    /// Whether the session was given a title, or its root a prompt with text.
    pub titled: bool,
    /// How many entries each agent's transcript holds.
    pub entries: BTreeMap<AgentId, u64>,
}

/// What `session` opens to.
pub(in crate::session) fn opening(db: &Db, session: SessionId) -> DbResult<Opening> {
    let agents = agent_paths(db, session, Scope::All)?;
    let agent = |row: i64| (agents.get(&row).cloned()).ok_or_else(|| corrupt("agent is missing"));
    let states = agent_states(db, &agents)?;
    let profiles = super::contract::profiles(db)?;
    let mut contexts: HashMap<AgentId, HashMap<_, _>> = HashMap::new();
    for (row, purpose, context, profile) in db.query(
        "SELECT x.agent, x.purpose, x.context, c.profile FROM agent_context x \
         JOIN model_context c ON c.entry = x.context",
        Vec::new(),
        |row| {
            let purpose: ModelPurpose = enum_column(row, 1)?;
            Ok((
                row.get(0)?,
                purpose,
                sequence(row.get(2)?),
                row.get::<i64>(3)?,
            ))
        },
    )? {
        let profile = (profiles.get(&profile).cloned())
            .ok_or_else(|| corrupt("a context names no profile"))?;
        let purposes = contexts.entry(agent(row)?).or_default();
        purposes.insert(purpose, (context, profile));
    }
    let mut unanswered = indexmap::IndexMap::new();
    for (row, message, call, name) in unanswered_calls(db)? {
        unanswered.insert((agent(row)?, call), (message, name));
    }
    let awaiting = "SELECT agent FROM agent_state WHERE awaiting";
    let awaiting = db.query(awaiting, Vec::new(), |row| agent(row.get(0)?))?;
    let awaiting = awaiting.into_iter().collect();
    let requests = named_requests(db, &agents, Named::Latest)?
        .into_iter()
        .collect();
    let ledger = ResidentLedger::opened(requests, contexts, unanswered, awaiting);
    let mut wanted = db.query(
        "SELECT m.entry FROM mode m \
         WHERE m.version = (SELECT max(version) FROM mode WHERE name = m.name) \
         UNION SELECT max(entry) FROM target_revision GROUP BY target",
        Vec::new(),
        entry,
    )?;
    wanted.extend(states.values().filter_map(|state| state.todos));
    let mut children = 0;
    let root = (agents.iter()).find(|(_, agent)| agent.parent().is_none());
    if let Some((&row, root)) = root
        && let Some(state) = states.get(root)
    {
        let context = ledger.context(root, ModelPurpose::Agent);
        wanted.extend(contract_entries(
            db,
            row,
            state.profiled,
            state.granted,
            context,
        )?);
        children = highest_child(db, row)?;
    }
    wanted.sort_unstable();
    wanted.dedup();
    let records = decode_records(db, session, Scope::Only(&wanted))?;
    let by_seq: HashMap<_, _> = (records.iter())
        .map(|record| (record.sequence, &**record))
        .collect();
    let mut todos = BTreeMap::new();
    for (row, owner_job, reconciled) in db.query(
        "SELECT a.id, a.owner_job, c.frontier FROM agent_state s JOIN agent a ON a.id = s.agent \
         LEFT JOIN compaction c ON c.entry = s.checkpoint",
        Vec::new(),
        |row| {
            let owner_job = row.get::<Option<i64>>(1)?.map(job).transpose()?;
            let frontier = row.get::<Option<i64>>(2)?.map(sequence).unwrap_or_default();
            Ok((row.get::<i64>(0)?, owner_job, frontier))
        },
    )? {
        let agent = agent(row)?;
        let set = states
            .get(&agent)
            .and_then(|state| by_seq.get(&state.todos?));
        todos.insert(agent, AgentTodos::opened(owner_job, reconciled));
        set.into_iter()
            .for_each(|record| todo::apply(&mut todos, record));
    }
    let resident = db.query(
        "WITH RECURSIVE chain(id) AS (SELECT job FROM job_status WHERE resident \
           UNION SELECT j.parent FROM job j JOIN chain ON j.id = chain.id \
           WHERE j.parent IS NOT NULL) \
         SELECT id FROM chain ORDER BY id",
        Vec::new(),
        |row| job(row.get(0)?),
    )?;
    let entries = db.query(
        "SELECT a.id, (SELECT coalesce(max(e.agent_ordinal) + 1, 0) FROM entry e \
           WHERE e.agent = a.id) FROM agent a",
        Vec::new(),
        |row| Ok((row.get::<i64>(0)?, u64_of(row.get(1)?))),
    )?;
    let entries = (entries.into_iter())
        .map(|(row, entries)| Ok((agent(row)?, entries)))
        .collect::<DbResult<_>>()?;
    Ok(Opening {
        gauged: named_requests(db, &agents, Named::Gauged)?,
        ledger,
        agents: states,
        records,
        children,
        todos,
        jobs: job_rows(db, session, &resident)?,
        seeds: job_seeds(db, &agents, &resident)?,
        titled: titled(db)?,
        entries,
    })
}

/// The calls committed since each agent's latest agent request that no result answers
/// yet, in commit order. Each agent's commits are read from its bound by index, never
/// by scanning the journal.
fn unanswered_calls(db: &Db) -> DbResult<Vec<(i64, MessageSeq, String, String)>> {
    db.query(
        format!(
            "SELECT s.agent, e.seq, c.call_id, c.name FROM agent_state s \
             CROSS JOIN entry e ON e.agent = s.agent AND e.kind = ?1 AND e.seq > {} \
             JOIN message_commit m ON m.entry = e.seq \
             JOIN assistant_item i ON i.message = m.message JOIN tool_call c ON c.item = i.id \
             WHERE NOT EXISTS (SELECT 1 FROM tool_result r WHERE r.call = c.item) \
             ORDER BY e.seq, i.position",
            since_agent_request("s.agent", "")
        ),
        params![
            EntryKind::MessageCommitted,
            EntryKind::ModelRequested,
            ModelPurpose::Agent
        ],
        |row| {
            Ok((
                row.get(0)?,
                sequence(row.get(1)?).message(),
                row.get(2)?,
                row.get(3)?,
            ))
        },
    )
}

/// The entries the contract of the agent at `row` is journaled in: its start, the
/// entries that applied its profile and its grants, its agent model context when that
/// pins the mode those grants apply, and the entry that recorded that mode's edition.
fn contract_entries(
    db: &Db,
    row: i64,
    profiled: RecordSeq,
    granted: RecordSeq,
    context: Option<RecordSeq>,
) -> DbResult<Vec<RecordSeq>> {
    let mut entries = vec![profiled, granted];
    entries.extend(db.query_row(
        "SELECT seq FROM entry WHERE agent = ?1 AND kind = ?2",
        params![row, EntryKind::AgentStarted],
        entry,
    )?);
    entries.extend(context.filter(|context| *context > granted));
    entries.extend(db.query_row(
        "SELECT m.entry FROM agent_mode am \
         JOIN mode m ON m.name = am.mode_name AND m.version = am.mode_version \
         WHERE am.entry = ?1",
        params![granted],
        entry,
    )?);
    Ok(entries)
}

/// The highest index among the children of the agent at `row`.
fn highest_child(db: &Db, row: i64) -> DbResult<u32> {
    let children = db.query_row(
        "SELECT coalesce(max(child_index), 0) FROM agent WHERE parent = ?1",
        params![row],
        |row| Ok(row.get::<u32>(0)?),
    )?;
    Ok(children.unwrap_or_default())
}

/// What an agent's journal leaves of its contract, read as its loop starts: the records
/// it is journaled in, in sequence order, and the highest index among its children.
pub(crate) struct AgentRecords {
    pub(crate) records: Vec<Arc<EventRecord>>,
    pub(crate) children: u32,
}

/// The contract of `agent`, whose `profiled` and `granted` entries and agent model
/// `context` its state names; none for an agent the journal never started.
pub(in crate::session) fn agent(
    db: &Db,
    session: SessionId,
    agent: &AgentId,
    [profiled, granted]: [RecordSeq; 2],
    context: Option<RecordSeq>,
) -> DbResult<Option<AgentRecords>> {
    let Some(row) = agent_row(db, agent)? else {
        return Ok(None);
    };
    let mut entries = contract_entries(db, row, profiled, granted, context)?;
    entries.sort_unstable();
    entries.dedup();
    let records = decode_records(db, session, Scope::Only(&entries))?;
    let children = highest_child(db, row)?;
    Ok(Some(AgentRecords { records, children }))
}

/// The jobs `agent` created, in id order.
pub(in crate::session) fn created_by(db: &Db, agent: &AgentId) -> DbResult<Vec<JobId>> {
    let Some(agent) = agent_row(db, agent)? else {
        return Ok(Vec::new());
    };
    db.query(
        "SELECT j.id FROM entry e JOIN job j ON j.created = e.seq \
         WHERE e.agent = ?1 AND e.kind = ?2 ORDER BY j.id",
        params![agent, EntryKind::JobCreated],
        |row| job(row.get(0)?),
    )
}

/// What the job folds count across the whole journal, for the `loaded` jobs:
/// every child launch and installed name (one each per child agent), the highest
/// id, and per owner how many jobs of the latest response it has loaded jobs of are
/// not loaded.
fn job_seeds(db: &Db, agents: &HashMap<i64, AgentId>, loaded: &[JobId]) -> DbResult<JobSeeds> {
    let agent =
        |row: i64| (agents.get(&row).cloned()).ok_or_else(|| corrupt("job agent is missing"));
    let last = db.query_row("SELECT coalesce(max(id), 0) FROM job", Vec::new(), |row| {
        Ok(u64_of(row.get(0)?))
    })?;
    let last = last.unwrap_or_default();
    let launches = db.query(
        // From the agents: one job per child, whatever the jobs' history.
        "SELECT a.id, j.id, e.agent, j.name FROM agent a CROSS JOIN job j ON j.id = a.owner_job \
         JOIN entry e ON e.seq = j.created ORDER BY a.owner_job",
        Vec::new(),
        |row| {
            let name = row.get::<Option<String>>(3)?.map(parsed).transpose()?;
            Ok((
                agent(row.get(0)?)?,
                job(row.get(1)?)?,
                agent(row.get(2)?)?,
                name,
            ))
        },
    )?;
    let ids: Vec<_> = loaded.iter().map(|job| job.get()).collect();
    let ids = json(&ids)?;
    let ended = db.query(
        "WITH latest(owner, response) AS ( \
           SELECT e.agent, max(c.entry) FROM job j JOIN entry e ON e.seq = j.created \
           JOIN assistant_item i ON i.id = j.origin_call \
           JOIN message_commit c ON c.message = i.message \
           WHERE j.id IN (SELECT value FROM json_each(?1)) GROUP BY e.agent) \
         SELECT l.owner, l.response, ( \
           SELECT count(*) FROM message_commit c JOIN assistant_item i ON i.message = c.message \
           JOIN job j ON j.origin_call = i.id WHERE c.entry = l.response) FROM latest l",
        params![ids],
        |row| {
            Ok((
                agent(row.get(0)?)?,
                sequence(row.get(1)?).message(),
                usize::try_from(row.get::<i64>(2)?).unwrap_or_default(),
            ))
        },
    )?;
    Ok(JobSeeds {
        last,
        launches,
        ended,
    })
}

/// Whether the session was titled or its root prompted with text, which its fold
/// keeps once either is journaled.
fn titled(db: &Db) -> DbResult<bool> {
    let titled = db.query_row("SELECT EXISTS (SELECT 1 FROM title)", Vec::new(), |row| {
        Ok(row.get::<bool>(0)?)
    })?;
    Ok(titled.unwrap_or_default() || super::state::root_prompt(db)?.is_some())
}

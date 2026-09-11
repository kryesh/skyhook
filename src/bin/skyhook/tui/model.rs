use super::{
    format::{agent_label, brief},
    tool_view::{Document, Role, Run, Section},
};
use serde_json::Value;
use skyhook::{
    agent::{AgentActivity, LiveResponse, ObservationSnapshot},
    identity::{AgentId, JobId},
    job::JobState,
    provider::protocol::{BlockContent, BlockKind, Message, ToolResult, Usage, UserContent},
    session::{ModelPurpose, SessionEvent},
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    time::Instant,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Conversation,
    Requests,
    Jobs,
}
impl Tab {
    pub fn next(self, backwards: bool) -> Self {
        let tabs = [Self::Conversation, Self::Requests, Self::Jobs];
        let n = tabs.iter().position(|t| *t == self).unwrap();
        tabs[(n + if backwards { tabs.len() - 1 } else { 1 }) % tabs.len()]
    }
}
#[derive(Default)]
pub struct View {
    pub tab: Tab,
    pub scroll: Option<usize>,
    pub row: usize,
    pub expanded: HashSet<String>,
    pub collapsed: HashSet<String>,
    pub query: String,
}
impl View {
    pub fn is_expanded(&self, key: &str, all: bool) -> bool {
        (all || self.expanded.contains(key)) && !self.collapsed.contains(key)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    User,
    Agent,
    Reasoning,
    Tool,
    Muted,
    Status,
    Error,
}
#[derive(Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub text: String,
    pub surface: Surface,
    pub expandable: bool,
    pub default_open: bool,
    pub running: bool,
    pub footer: Option<String>,
    /// Metadata-only request row, laid out with shared columns.
    pub request: Option<RequestRow>,
    pub indent: u16,
    pub job: Option<JobId>,
    pub document: Option<Document>,
    /// Structured presentation-only header, shared by collapsed and expanded cards.
    pub header: Option<Vec<Run>>,
    /// Omit the separator before a related sibling tool or this script's first child.
    pub compact_after: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub struct RequestRow {
    pub sequence: u64,
    pub purpose: ModelPurpose,
    pub model: String,
    pub status: &'static str,
    pub usage: Option<Usage>,
    pub elapsed_tenths: Option<u64>,
}

impl RequestRow {
    pub fn metadata(&self) -> [String; 4] {
        [
            format!("Request #{}", self.sequence),
            format!("{:?}", self.purpose),
            self.model.clone(),
            self.status.into(),
        ]
    }

    pub fn statistics(&self) -> [String; 4] {
        let tokens = |value: fn(Usage) -> String| self.usage.map_or_else(|| "—".into(), value);
        [
            tokens(|usage| number(usage.output_tokens)),
            tokens(|usage| number(usage.input_tokens)),
            tokens(|usage| number(usage.cached_input_tokens)),
            self.elapsed_tenths.map_or_else(
                || "—".into(),
                |tenths| format!("{}.{}s", tenths / 10, tenths % 10),
            ),
        ]
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolGroup {
    Response(u64),
    Script(JobId),
    Notification(u64, usize),
}

impl Entry {
    fn new(key: String, text: String, surface: Surface) -> Self {
        Self {
            key,
            text,
            surface,
            expandable: false,
            default_open: false,
            running: false,
            footer: None,
            request: None,
            indent: 0,
            job: None,
            document: None,
            header: None,
            compact_after: false,
        }
    }
}
#[derive(Clone)]
pub struct AgentInfo {
    pub id: AgentId,
    pub name: String,
    pub model: String,
    pub target: String,
    pub owner: Option<JobId>,
    pub terminal: bool,
}
#[derive(Clone)]
pub struct JobInfo {
    pub id: JobId,
    pub agent: AgentId,
    pub name: Option<String>,
    pub tool: String,
    pub args: Value,
    pub parent: Option<JobId>,
    pub state: JobState,
    pub target: String,
    pub location: String,
    pub remote: bool,
    pub error: Option<String>,
}

#[derive(Default)]
struct RequestInfo {
    started_millis: Option<i64>,
    finished_millis: Option<i64>,
    usage: Option<Usage>,
    model: Option<String>,
    failed: bool,
    response: Option<u64>,
}

#[derive(Default)]
pub struct Projection {
    pub agents: Vec<AgentInfo>,
    pub jobs: BTreeMap<JobId, JobInfo>,
    pub usage: Usage,
    pub agent_usage: HashMap<AgentId, Usage>,
    pub completed: HashMap<AgentId, Instant>,
    through: u64,
    records_by_agent: HashMap<AgentId, Vec<u64>>,
    requests: HashMap<u64, RequestInfo>,
    active_request: HashMap<AgentId, u64>,
    response_requests: HashMap<u64, u64>,
    tool_origins: HashSet<(AgentId, u64, String)>,
}
impl Projection {
    fn response_committed(&self, request: u64) -> bool {
        self.requests
            .get(&request)
            .is_some_and(|r| r.response.is_some())
    }

    fn live_response(&self, request: u64, response: &LiveResponse) -> bool {
        // Settlement can announce a journal sequence before its record arrives.
        // Keep that snapshot visible until the projection has consumed the commit.
        (!response.settled || response.message.is_some()) && !self.response_committed(request)
    }

    pub fn rebuild(&mut self, snapshot: &ObservationSnapshot) {
        for (_, record) in snapshot.records.range((self.through + 1)..) {
            self.through = record.sequence;
            self.records_by_agent
                .entry(record.agent.clone())
                .or_default()
                .push(record.sequence);
            match &record.event {
                SessionEvent::AgentStarted {
                    model_profile,
                    location,
                    owner_job,
                    ..
                } => {
                    self.agents.retain(|agent| agent.id != record.agent);
                    self.agents.push(AgentInfo {
                        id: record.agent.clone(),
                        name: if record.agent.path().is_empty() {
                            "skyhook".into()
                        } else {
                            format!("agent {}", agent_label(&record.agent))
                        },
                        model: model_profile.clone(),
                        target: location.target.clone(),
                        owner: *owner_job,
                        terminal: false,
                    });
                    self.completed.remove(&record.agent);
                }
                SessionEvent::JobCreated {
                    job,
                    tool,
                    name,
                    arguments,
                    parent,
                    location,
                    ..
                } => {
                    self.jobs.insert(
                        *job,
                        JobInfo {
                            id: *job,
                            agent: record.agent.clone(),
                            name: name.clone(),
                            tool: tool.clone(),
                            args: arguments.clone(),
                            parent: *parent,
                            state: JobState::Queued,
                            target: location.target.clone(),
                            location: format!(
                                "{} · {}",
                                location.target,
                                location.workspace.display()
                            ),
                            remote: location.target != "root",
                            error: None,
                        },
                    );
                }
                SessionEvent::JobStateChanged { job, state } => {
                    if let Some(info) = self.jobs.get_mut(job) {
                        info.state = *state;
                    }
                    // Resuming a retained child reuses its owner job and does not
                    // emit AgentStarted again. Reopen its tree row on that job's
                    // Running event, not on ordinary model/tool activity updates.
                    if *state == JobState::Running {
                        for agent in &mut self.agents {
                            if agent.owner == Some(*job) && agent.terminal {
                                agent.terminal = false;
                                self.completed.remove(&agent.id);
                            }
                        }
                    }
                }
                SessionEvent::JobFinished {
                    job, state, error, ..
                } => {
                    if let Some(info) = self.jobs.get_mut(job) {
                        info.state = *state;
                        info.error = error.clone();
                    }
                }
                SessionEvent::AgentCompleted | SessionEvent::AgentInterrupted => {
                    if let Some(info) = self.agents.iter_mut().find(|a| a.id == record.agent) {
                        info.terminal = true;
                    }
                    self.completed
                        .entry(record.agent.clone())
                        .or_insert_with(Instant::now);
                }
                SessionEvent::ModelChanged { model_profile, .. } => {
                    if let Some(agent) = self.agents.iter_mut().find(|a| a.id == record.agent) {
                        agent.model.clone_from(model_profile);
                    }
                }
                SessionEvent::Usage { usage, .. } => {
                    add_usage(&mut self.usage, *usage);
                    add_usage(
                        self.agent_usage.entry(record.agent.clone()).or_default(),
                        *usage,
                    );
                }
                _ => {}
            }
            match &record.event {
                SessionEvent::ModelRequested { context, .. } => {
                    let model =
                        snapshot
                            .records
                            .get(context)
                            .and_then(|record| match &record.event {
                                SessionEvent::ModelContext { template, .. } => {
                                    Some(template.model.clone())
                                }
                                _ => None,
                            });
                    let info = self.requests.entry(record.sequence).or_default();
                    info.model = model;
                    info.started_millis = Some(record.timestamp_millis);
                    self.active_request
                        .insert(record.agent.clone(), record.sequence);
                }
                SessionEvent::AgentInterrupted => {
                    if let Some(request) = self.active_request.get(&record.agent)
                        && let Some(info) = self.requests.get_mut(request)
                    {
                        info.finished_millis.get_or_insert(record.timestamp_millis);
                    }
                }
                SessionEvent::ModelFailed { request, .. } => {
                    let info = self.requests.entry(*request).or_default();
                    info.failed = true;
                    info.finished_millis.get_or_insert(record.timestamp_millis);
                }
                SessionEvent::Usage {
                    request: Some(request),
                    usage,
                } => {
                    let info = self.requests.entry(*request).or_default();
                    add_usage(info.usage.get_or_insert_with(Usage::default), *usage);
                    info.finished_millis.get_or_insert(record.timestamp_millis);
                }
                SessionEvent::MessageCommitted {
                    message: Message::Assistant(_),
                } => {
                    if let Some(&request_id) = self.active_request.get(&record.agent) {
                        let request = self.requests.entry(request_id).or_default();
                        if request.response.is_none() {
                            self.response_requests.insert(record.sequence, request_id);
                            request.response = Some(record.sequence);
                            request
                                .finished_millis
                                .get_or_insert(record.timestamp_millis);
                        }
                    }
                }
                SessionEvent::JobCreated {
                    origin: Some(origin),
                    ..
                } => {
                    self.tool_origins.insert((
                        record.agent.clone(),
                        origin.message,
                        origin.call_id.clone(),
                    ));
                }
                _ => {}
            }
        }
        for agent in &mut self.agents {
            if let Some(job) = agent.owner.and_then(|id| self.jobs.get(&id)) {
                if let Some(name) = &job.name {
                    agent.name = name.clone();
                }
                if job.state.is_terminal() && !agent.terminal {
                    agent.terminal = true;
                    self.completed
                        .entry(agent.id.clone())
                        .or_insert_with(Instant::now);
                }
            }
        }
        self.agents.sort_by(|a, b| a.id.path().cmp(b.id.path()));
    }
    pub fn has_active_children(&self) -> bool {
        self.agents
            .iter()
            .any(|agent| !agent.id.path().is_empty() && !agent.terminal)
    }
    fn child_target<'a>(&'a self, caller: &AgentId, arguments: &'a Value) -> &'a str {
        arguments
            .get("target")
            .and_then(Value::as_str)
            .or_else(|| {
                self.agents
                    .iter()
                    .find(|agent| &agent.id == caller)
                    .map(|agent| agent.target.as_str())
            })
            .unwrap_or("root")
    }
    fn job_target<'a>(&'a self, job: &'a JobInfo) -> &'a str {
        if job.tool == "agent" {
            self.agents
                .iter()
                .find(|agent| agent.owner == Some(job.id))
                .map_or_else(
                    || self.child_target(&job.agent, &job.args),
                    |agent| agent.target.as_str(),
                )
        } else {
            &job.target
        }
    }
    pub fn visible(&self, selected: &AgentId) -> Vec<AgentInfo> {
        self.agents
            .iter()
            .filter(|a| {
                a.id.path().is_empty()
                    || !a.terminal
                    || selected.path().starts_with(a.id.path())
                    || self
                        .completed
                        .get(&a.id)
                        .is_some_and(|t| t.elapsed().as_secs() < 2)
            })
            .cloned()
            .collect()
    }
    pub fn status(&self, agent: &AgentInfo, snapshot: &ObservationSnapshot) -> (bool, String) {
        if agent.terminal {
            return (
                false,
                agent
                    .owner
                    .and_then(|j| self.jobs.get(&j))
                    .map_or("Completed".into(), |j| state_name(j.state).into()),
            );
        }
        if let Some(owner) = agent.owner.and_then(|id| self.jobs.get(&id))
            && owner.state == JobState::WaitingInput
        {
            return (false, "Waiting for parent input".into());
        }
        match snapshot.activity.get(&agent.id) {
            Some(AgentActivity::Working) => (true, "Working".into()),
            Some(AgentActivity::Reconnecting {
                attempt,
                max_attempts,
            }) => (
                true,
                format!("Reconnecting · attempt {attempt} of {max_attempts}"),
            ),
            Some(AgentActivity::Compacting) => (true, "Compacting".into()),
            Some(AgentActivity::Interrupted) => (false, "Interrupted".into()),
            Some(AgentActivity::Failed(_)) => (false, "Failed".into()),
            Some(AgentActivity::WaitingChildren) => (false, "Waiting for child".into()),
            activity => {
                let jobs: Vec<_> = self
                    .jobs
                    .values()
                    .filter(|j| j.agent == agent.id && !j.state.is_terminal())
                    .collect();
                if !jobs.is_empty() && jobs.iter().all(|j| j.state == JobState::AwaitingApproval) {
                    (false, "Waiting for permission".into())
                } else if jobs
                    .iter()
                    .any(|j| j.tool == "ask" || j.state == JobState::WaitingInput)
                {
                    (false, "Waiting for input".into())
                } else if !jobs.is_empty() && jobs.iter().all(|j| j.tool == "agent") {
                    (false, "Waiting for child".into())
                } else if matches!(activity, Some(AgentActivity::Tools)) || !jobs.is_empty() {
                    (true, "Running tools".into())
                } else {
                    (false, "Ready".into())
                }
            }
        }
    }
}
fn add_usage(sum: &mut Usage, value: Usage) {
    sum.input_tokens = sum.input_tokens.saturating_add(value.input_tokens);
    sum.cached_input_tokens = sum
        .cached_input_tokens
        .saturating_add(value.cached_input_tokens);
    sum.output_tokens = sum.output_tokens.saturating_add(value.output_tokens);
}
pub fn state_name(state: JobState) -> &'static str {
    match state {
        JobState::Queued => "Queued",
        JobState::AwaitingApproval => "Waiting for permission",
        JobState::Running => "Running",
        JobState::WaitingInput => "Waiting for input",
        JobState::Completed => "Completed",
        JobState::Failed => "Failed",
        JobState::Cancelled => "Cancelled",
        JobState::Interrupted => "Interrupted",
    }
}
fn state_role(state: JobState) -> Role {
    match state {
        JobState::Running => Role::Indicator,
        JobState::AwaitingApproval | JobState::WaitingInput => Role::Warning,
        JobState::Completed => Role::Success,
        JobState::Failed => Role::Error,
        JobState::Queued | JobState::Cancelled | JobState::Interrupted => Role::Muted,
    }
}

fn header_text(runs: &[Run]) -> String {
    runs.iter().map(Run::text).collect()
}

pub fn agent_footer(
    snapshot: &ObservationSnapshot,
    projection: &Projection,
    agent: &AgentId,
) -> String {
    agent_footer_stats(snapshot, projection, agent).join(" · ")
}
pub fn agent_footer_stats(
    snapshot: &ObservationSnapshot,
    projection: &Projection,
    agent: &AgentId,
) -> [String; 3] {
    super::format::footer_stats(
        projection
            .agent_usage
            .get(agent)
            .copied()
            .unwrap_or_default(),
        snapshot
            .context
            .get(agent)
            .map(|context| (context.tokens, context.capacity)),
    )
}
pub use super::format::{clean, footer, number, pretty};

/// Indices refer to the caller-owned entry vector. `appends` maps a subset of
/// `dirty` to their previous text byte lengths; their prefixes are unchanged.
#[derive(Default, Debug)]
pub struct ContentChanges {
    pub dirty: Vec<usize>,
    pub appends: HashMap<usize, usize>,
    pub reset: bool,
}

/// Retains rendered journal content across streaming events. The caller bumps
/// `revision` for history and presentation changes. ResponseEvent mutations use
/// `observe_response` so authoritative replacements rebuild only the live tail.
/// Journal progress, selected agent, tab and defaults are also checked here.
///
/// Entries remain caller-owned: neither historical strings nor tool documents
/// are cloned to hand the cache's result to the renderer.
#[derive(Default)]
pub struct ContentCache {
    identity: Option<(AgentId, Tab, bool, bool, u64, u64)>,
    history_len: usize,
    history_running: bool,
    live: Vec<LiveContent>,
    dirty_responses: HashMap<AgentId, HashSet<u64>>,
    job_indices: HashMap<JobId, usize>,
    invalid_jobs: HashSet<JobId>,
}

struct LiveContent {
    request: u64,
    start: usize,
    count: usize,
}

#[derive(Clone, Copy)]
pub struct EntryView<'a> {
    pub agent: &'a AgentId,
    pub view: &'a View,
    pub thinking: bool,
    pub all_details: bool,
}

impl ContentCache {
    /// Response events can replace content, reorder blocks, or end a block
    /// without changing its text. Never infer cache validity from string lengths.
    pub fn observe_response(&mut self, agent: &AgentId, request: u64) {
        self.dirty_responses
            .entry(agent.clone())
            .or_default()
            .insert(request);
    }

    /// Refresh downloaded output for one tool without invalidating history.
    pub fn invalidate_job(&mut self, job: JobId) {
        self.invalid_jobs.insert(job);
    }

    fn finish_reset(
        &mut self,
        entries: &mut [Entry],
        old: Vec<Entry>,
        changes: &mut ContentChanges,
    ) {
        self.job_indices.clear();
        for (index, entry) in entries.iter().enumerate() {
            if let Some(job) = entry.job {
                self.job_indices.insert(job, index);
            }
        }
        self.invalid_jobs.clear();
        // Preserve warm layout for journal/output changes that leave ordering
        // intact, including response snapshot replacements. Reuse equal
        // allocations and retain layout for unchanged entries.
        changes.reset =
            old.is_empty() || old.iter().zip(entries.iter()).any(|(a, b)| a.key != b.key);
        changes.dirty.clear();
        changes.appends.clear();
        if !changes.reset {
            let old_len = old.len();
            for (index, previous) in old.into_iter().enumerate().take(entries.len()) {
                if previous == entries[index] {
                    entries[index] = previous;
                } else {
                    changes.dirty.push(index);
                }
            }
            changes.dirty.extend(old_len..entries.len());
        }
    }

    pub fn update(
        &mut self,
        entries: &mut Vec<Entry>,
        snapshot: &ObservationSnapshot,
        projection: &Projection,
        presentation: EntryView<'_>,
        outputs: &HashMap<JobId, Value>,
        revision: u64,
    ) -> ContentChanges {
        let EntryView {
            agent,
            view,
            thinking,
            all_details,
        } = presentation;
        let identity = (
            agent.clone(),
            view.tab,
            thinking,
            all_details,
            revision,
            projection.through,
        );
        let reset = self.identity.as_ref() != Some(&identity);
        let dirty_responses = self.dirty_responses.remove(agent).unwrap_or_default();
        let mut changes = ContentChanges {
            reset,
            ..ContentChanges::default()
        };
        let old = if reset {
            Some(std::mem::take(entries))
        } else {
            None
        };
        let agent_name = projection
            .agents
            .iter()
            .find(|a| &a.id == agent)
            .map_or("Agent", |a| a.name.as_str());
        if reset {
            self.identity = Some(identity);
            *entries = entries_inner(snapshot, projection, presentation, outputs, false);
            self.history_len = entries.len();
            self.history_running = entries.iter().any(|entry| entry.running);
            self.live.clear();
        }
        if !reset {
            for job in self.invalid_jobs.drain() {
                if let Some(&index) = self.job_indices.get(&job)
                    && let Some(info) = projection.jobs.get(&job)
                {
                    let compact_after = entries[index].compact_after;
                    entries[index] = job_entry(info, projection, view, outputs, all_details);
                    entries[index].compact_after = compact_after;
                    changes.dirty.push(index);
                }
            }
        }
        if view.tab == Tab::Requests && !reset {
            // Elapsed time changes without a new journal record.
            if let Some(&request) = projection.active_request.get(agent)
                && let Some(info) = projection.requests.get(&request)
                && let Some(index) = entries
                    .iter()
                    .position(|entry| entry.key == format!("r{request}"))
            {
                let running = request_running(info, snapshot, projection, agent, request);
                let entry = &mut entries[index];
                let elapsed_tenths = request_elapsed(info, running);
                if entry.running != running {
                    if let Some(record) = snapshot.records.get(&request)
                        && let SessionEvent::ModelRequested { purpose, .. } = &record.event
                    {
                        *entry = request_entry(request, purpose, info, running);
                        changes.dirty.push(index);
                    }
                } else if let Some(row) = &mut entry.request
                    && row.elapsed_tenths != elapsed_tenths
                {
                    row.elapsed_tenths = elapsed_tenths;
                    changes.dirty.push(index);
                }
            }
        }
        if view.tab != Tab::Conversation {
            if let Some(old) = old {
                self.finish_reset(entries, old, &mut changes);
            }
            return changes;
        }
        if reset {
            let mut responses: Vec<_> = snapshot
                .responses
                .iter()
                .filter(|((owner, request), response)| {
                    owner == agent && projection.live_response(*request, response)
                })
                .collect();
            responses.sort_by_key(|((_, request), _)| *request);
            for ((_, request), response) in responses {
                let start = entries.len();
                entries.extend(response_entries(
                    *request, response, view, thinking, agent_name,
                ));
                self.live.push(LiveContent {
                    request: *request,
                    start,
                    count: entries.len() - start,
                });
            }
        }
        if !reset && !dirty_responses.is_empty() {
            // Native events can replace equal-length text, reorder items, or end
            // blocks. Rebuild only the live suffix, never clone journal entries.
            let previous = entries.split_off(self.history_len);
            let mut requests: Vec<_> = self
                .live
                .iter()
                .map(|live| live.request)
                .chain(dirty_responses.iter().copied())
                .collect();
            requests.sort_unstable();
            requests.dedup();
            self.live.clear();
            for request in requests {
                if let Some(response) = snapshot.responses.get(&(agent.clone(), request))
                    && projection.live_response(request, response)
                {
                    let start = entries.len();
                    entries.extend(response_entries(
                        request, response, view, thinking, agent_name,
                    ));
                    self.live.push(LiveContent {
                        request,
                        start,
                        count: entries.len() - start,
                    });
                }
            }
            // Keep unchanged live allocations, too. The old working indicator
            // is regenerated below; entry removal is conveyed by vector length.
            let reordered = previous
                .iter()
                .zip(&entries[self.history_len..])
                .any(|(old, new)| old.key != new.key);
            changes.reset |= reordered;
            let previous_len = previous.len();
            for (offset, old) in previous.into_iter().enumerate() {
                let index = self.history_len + offset;
                if let Some(entry) = entries.get_mut(index) {
                    if *entry == old {
                        *entry = old;
                    } else {
                        changes.dirty.push(index);
                    }
                }
            }
            changes
                .dirty
                .extend(self.history_len + previous_len..entries.len());
        }
        // The working indicator is a synthetic tail, never part of history.
        let end = self
            .live
            .last()
            .map_or(self.history_len, |l| l.start + l.count);
        let working_key = format!("working-{agent}");
        let running = self.history_running
            || entries[self.history_len..end]
                .iter()
                .any(|entry| entry.running);
        let has_working = entries.get(end).is_some_and(|e| e.key == working_key);
        if let Some(label) = working_label(snapshot, projection, agent, running) {
            let entry = working_entry(agent, &label);
            if !has_working {
                entries.insert(end, entry);
                changes.dirty.extend(end..entries.len());
            } else if entries[end] != entry {
                entries[end] = entry;
                changes.dirty.push(end);
            }
        } else if has_working {
            entries.remove(end);
            changes.dirty.extend(end..=entries.len());
        }
        if let Some(old) = old {
            self.finish_reset(entries, old, &mut changes);
        }
        changes.dirty.sort_unstable();
        changes.dirty.dedup();
        changes.appends.retain(|index, _| *index < entries.len());
        changes
    }
}

pub fn entries(
    snapshot: &ObservationSnapshot,
    projection: &Projection,
    agent: &AgentId,
    view: &View,
    outputs: &HashMap<JobId, Value>,
    thinking: bool,
    all_details: bool,
) -> Vec<Entry> {
    entries_inner(
        snapshot,
        projection,
        EntryView {
            agent,
            view,
            thinking,
            all_details,
        },
        outputs,
        true,
    )
}

fn entries_inner(
    snapshot: &ObservationSnapshot,
    projection: &Projection,
    presentation: EntryView<'_>,
    outputs: &HashMap<JobId, Value>,
    include_live: bool,
) -> Vec<Entry> {
    let EntryView {
        agent,
        view,
        thinking,
        all_details,
    } = presentation;
    let records: Vec<_> = projection
        .records_by_agent
        .get(agent)
        .into_iter()
        .flatten()
        .filter_map(|sequence| snapshot.records.get(sequence))
        .collect();
    match view.tab {
        Tab::Requests => records
            .iter()
            .filter_map(|r| {
                if let SessionEvent::ModelRequested { purpose, .. } = &r.event {
                    let info = projection.requests.get(&r.sequence)?;
                    let running = request_running(info, snapshot, projection, agent, r.sequence);
                    Some(request_entry(r.sequence, purpose, info, running))
                } else {
                    None
                }
            })
            .collect(),
        Tab::Jobs => projection
            .jobs
            .values()
            .filter(|j| &j.agent == agent)
            .map(|job| job_entry(job, projection, view, outputs, all_details))
            .collect(),
        Tab::Conversation => {
            let recoveries: HashSet<_> = records
                .iter()
                .filter_map(|record| match &record.event {
                    SessionEvent::ModelRecoveryScheduled { request, .. } => Some(*request),
                    _ => None,
                })
                .collect();
            // Tool results have an explicit call ID but no message sequence. Resolve
            // only within the current agent's most recent assistant turn, consuming
            // each call once. Never let an old/reused ID suppress a later result.
            let mut pending = HashMap::new();
            let mut turn = None;
            let mut call_results = HashMap::new();
            let mut matched_results = HashSet::new();
            for record in &records {
                match &record.event {
                    SessionEvent::MessageCommitted {
                        message: Message::Assistant(items),
                    } => {
                        pending.clear();
                        turn = Some(record.sequence);
                        for block in items.iter().flat_map(|item| &item.blocks) {
                            if let BlockContent::ToolCall(call) = &block.content {
                                pending
                                    .insert((call.id.clone(), call.name.clone()), record.sequence);
                            }
                        }
                    }
                    SessionEvent::JobCreated {
                        origin: Some(origin),
                        tool,
                        ..
                    } if turn.is_none_or(|turn| turn <= origin.message) => {
                        // Retained job provenance also identifies a call whose
                        // assistant message is no longer in the retained history.
                        pending.insert((origin.call_id.clone(), tool.clone()), origin.message);
                        turn = Some(origin.message);
                    }
                    SessionEvent::MessageCommitted {
                        message: Message::Tool(results),
                    } => {
                        for (index, result) in results.iter().enumerate() {
                            if let Some(message) =
                                pending.remove(&(result.call_id.clone(), result.name.clone()))
                            {
                                call_results.insert((message, result.call_id.clone()), result);
                                matched_results.insert((record.sequence, index));
                            }
                        }
                    }
                    _ => {}
                }
            }
            let mut entries = Vec::new();
            let mut tool_groups = HashMap::new();
            let agent_name = projection
                .agents
                .iter()
                .find(|a| &a.id == agent)
                .map_or("Agent", |a| a.name.as_str());
            for record in &records {
                let key = format!("m{}", record.sequence);
                match &record.event {
                    SessionEvent::ModelRequested { .. } => {
                        if let Some(response) =
                            snapshot.responses.get(&(agent.clone(), record.sequence))
                            && response.settled
                            && !projection.live_response(record.sequence, response)
                            && !projection.response_committed(record.sequence)
                        {
                            entries.extend(response_entries(
                                record.sequence,
                                response,
                                view,
                                thinking,
                                agent_name,
                            ));
                        }
                    }
                    SessionEvent::MessageCommitted { message } => match message {
                        Message::User(blocks) => {
                            for (i, block) in blocks.iter().enumerate() {
                                let (text, surface) = match block {
                                    UserContent::Text { text } => (
                                        format!(
                                            "{}\n{text}",
                                            if agent.path().is_empty() {
                                                "You"
                                            } else {
                                                "Parent"
                                            }
                                        ),
                                        Surface::User,
                                    ),
                                    UserContent::ParentInput { text } => {
                                        (format!("Parent\n{text}"), Surface::User)
                                    }
                                    UserContent::Image { image } => (
                                        format!("Image attachment\n{}", pretty(image)),
                                        Surface::User,
                                    ),
                                    UserContent::Runtime { text }
                                        if job_notification_kind(text).is_some() =>
                                    {
                                        for entry in job_event_entries(
                                            &format!("{key}/{i}"),
                                            text,
                                            projection,
                                            view,
                                            all_details,
                                        ) {
                                            tool_groups.insert(
                                                entry.key.clone(),
                                                ToolGroup::Notification(record.sequence, i),
                                            );
                                            entries.push(entry);
                                        }
                                        continue;
                                    }
                                    UserContent::Runtime { text } => {
                                        (format!("Harness notification\n{text}"), Surface::Muted)
                                    }
                                    UserContent::Compaction { text } => {
                                        (format!("Compaction\n{text}"), Surface::Muted)
                                    }
                                };
                                entries.push(Entry::new(format!("{key}/{i}"), text, surface));
                            }
                        }
                        Message::Assistant(items) => {
                            let request = projection
                                .response_requests
                                .get(&record.sequence)
                                .copied()
                                .unwrap_or(record.sequence);
                            let blocks: Vec<_> = items
                                .iter()
                                .flat_map(|item| item.blocks.iter().map(move |block| (item, block)))
                                .collect();
                            let final_text = if blocks.iter().any(|(_, block)| {
                                matches!(&block.content, BlockContent::ToolCall(_))
                            }) {
                                None
                            } else {
                                blocks.iter().rposition(|(_, block)| {
                                    matches!(&block.content, BlockContent::Text { text } if !text.trim().is_empty())
                                })
                            };
                            for (i, (item, block)) in blocks.iter().enumerate() {
                                let block_key = response_block_key(request, &item.id, &block.id);
                                match &block.content {
                                    BlockContent::Text { text } if !text.trim().is_empty() => {
                                        let mut entry = Entry::new(
                                            block_key.clone(),
                                            format!("{agent_name}\n{text}"),
                                            Surface::Agent,
                                        );
                                        if Some(i) == final_text {
                                            entry.footer = projection
                                                .response_requests
                                                .get(&record.sequence)
                                                .and_then(|request| {
                                                    projection.requests.get(request)
                                                })
                                                .and_then(|request| request.model.clone());
                                        }
                                        entries.push(entry);
                                    }
                                    BlockContent::Reasoning { text, .. }
                                        if !text.trim().is_empty() =>
                                    {
                                        entries.push(reasoning_entry(
                                            reasoning_key(request, &item.id, &block.id),
                                            text,
                                            view,
                                            thinking,
                                            "Reasoning",
                                        ));
                                    }
                                    BlockContent::ToolCall(call) => {
                                        let exists = projection
                                            .tool_origins
                                            .contains(&(agent.clone(), record.sequence, call.id.clone()));
                                        if !exists {
                                            let e = call_entry(
                                                block_key.clone(), &call.name, Some(&call.arguments),
                                                call_results.get(&(record.sequence, call.id.clone())).copied(),
                                                agent, projection, view, all_details,
                                            );
                                            tool_groups.insert(
                                                e.key.clone(),
                                                ToolGroup::Response(record.sequence),
                                            );
                                            entries.push(e);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Message::Tool(results) => {
                            for (index, result) in results.iter().enumerate() {
                                if !matched_results.contains(&(record.sequence, index)) {
                                    entries.push(call_entry(
                                        format!("{key}/{}", result.call_id), &result.name, None,
                                        Some(result), agent, projection, view, all_details,
                                    ));
                                }
                            }
                        }
                    },
                    SessionEvent::JobCreated { job, origin, .. } => {
                        if let Some(job) = projection.jobs.get(job) {
                            let entry = job_entry(job, projection, view, outputs, all_details);
                            // Script children belong to their immediate script,
                            // not to the model response that launched the script.
                            let group = job
                                .parent
                                .and_then(|parent| projection.jobs.get(&parent))
                                .filter(|parent| parent.tool == "script")
                                .map(|parent| ToolGroup::Script(parent.id))
                                .or_else(|| {
                                    origin
                                        .as_ref()
                                        .map(|origin| ToolGroup::Response(origin.message))
                                });
                            if let Some(group) = group {
                                tool_groups.insert(entry.key.clone(), group);
                            }
                            entries.push(entry);
                        }
                    }
                    SessionEvent::Compaction { checkpoint } => {
                        let key = format!("c{}", record.sequence);
                        let open = view.expanded.contains(&key);
                        let mut e = Entry::new(
                            key,
                            format!(
                                "{} Context compacted · {} → {}{}",
                                if open { "▾" } else { "▸" },
                                number(checkpoint.before_tokens),
                                number(checkpoint.after_tokens),
                                if open {
                                    format!(
                                        "\nSummary and retained sources\n{}",
                                        pretty(checkpoint)
                                    )
                                } else {
                                    String::new()
                                }
                            ),
                            Surface::Muted,
                        );
                        e.expandable = true;
                        entries.push(e);
                    }
                    SessionEvent::Status { message } => entries.push(Entry::new(
                        key,
                        format!("Status · {message}"),
                        Surface::Status,
                    )),
                    SessionEvent::ModelFailed {
                        attempt,
                        error,
                        request,
                    } => {
                        if recoveries.contains(request) {
                            // The recovery status below represents this failure;
                            // keep partial output, but do not imply a terminal turn.
                            continue;
                        }
                        // This is the logical invocation count, not the provider's
                        // internal HTTP retry count. Do not promise a fixed /3 here.
                        // Exhausted HTTP retries report their count in the error.
                        let label = if *attempt == 1 {
                            "Request failed".to_owned()
                        } else {
                            format!("Request failed · attempt {attempt}")
                        };
                        entries.push(Entry::new(
                            format!("failed{request}"),
                            format!("{label}\n{error}"),
                            Surface::Error,
                        ));
                    }
                    SessionEvent::ModelRecoveryScheduled {
                        attempt,
                        max_attempts,
                        delay_millis,
                        error,
                        ..
                    } => entries.push(Entry::new(
                        key,
                        format!(
                            "Reconnecting · attempt {attempt} of {max_attempts} · retry delay {delay_millis} ms\n{error}"
                        ),
                        Surface::Status,
                    )),
                    SessionEvent::CompactionFailed { error, .. } => entries.push(Entry::new(
                        key,
                        format!("Compaction failed; previous context retained\n{error}"),
                        Surface::Error,
                    )),
                    SessionEvent::CompactionSkipped { reason, .. } => entries.push(Entry::new(
                        key,
                        format!("Compaction skipped · {reason}"),
                        Surface::Muted,
                    )),
                    _ => {}
                }
            }
            // Store adjacency on the preceding entry so equality-based cache
            // invalidation also relayouts it when a sibling arrives or disappears.
            for index in 0..entries.len().saturating_sub(1) {
                let next_group = tool_groups.get(&entries[index + 1].key);
                let script_child = entries[index]
                    .job
                    .is_some_and(|job| next_group == Some(&ToolGroup::Script(job)));
                entries[index].compact_after = script_child
                    || tool_groups
                        .get(&entries[index].key)
                        .is_some_and(|group| next_group == Some(group));
            }
            if !include_live {
                return entries;
            }
            let mut responses: Vec<_> = snapshot
                .responses
                .iter()
                .filter(|((owner, request), response)| {
                    owner == agent && projection.live_response(*request, response)
                })
                .collect();
            responses.sort_by_key(|((_, request), _)| *request);
            entries.extend(responses.into_iter().flat_map(|((_, request), response)| {
                response_entries(*request, response, view, thinking, agent_name)
            }));
            if let Some(label) = working_label(
                snapshot,
                projection,
                agent,
                entries.iter().any(|entry| entry.running),
            ) {
                entries.push(working_entry(agent, &label));
            }
            entries
        }
    }
}
/// Shared by retained UI content and fresh export construction.
fn working_label(
    snapshot: &ObservationSnapshot,
    projection: &Projection,
    agent: &AgentId,
    running: bool,
) -> Option<String> {
    if running {
        return None;
    }
    let label = match snapshot.activity.get(agent) {
        Some(AgentActivity::Working)
            if !projection
                .active_request
                .get(agent)
                .is_some_and(|request| projection.response_committed(*request)) =>
        {
            "Working".into()
        }
        Some(AgentActivity::Reconnecting {
            attempt,
            max_attempts,
        }) => format!("Reconnecting · attempt {attempt} of {max_attempts}"),
        Some(AgentActivity::Compacting) => "Compacting".into(),
        _ => return None,
    };
    Some(label)
}

fn working_entry(agent: &AgentId, label: &str) -> Entry {
    let mut entry = Entry::new(
        format!("working-{agent}"),
        format!("  {label}"),
        Surface::Muted,
    );
    entry.running = true;
    entry
}

fn reasoning_entry(key: String, text: &str, view: &View, default_open: bool, title: &str) -> Entry {
    // Source lines determine collapsibility; terminal wrapping must not change interaction.
    let text = text.trim_matches(['\r', '\n']);
    if text.lines().count() <= 1 {
        let mut entry = Entry::new(key, text.to_owned(), Surface::Reasoning);
        entry.default_open = default_open;
        return entry;
    }
    let open = view.is_expanded(&key, default_open);
    let mut entry = Entry::new(
        key,
        if open {
            format!("▾ {title}\n{text}")
        } else {
            format!("▸ {title}")
        },
        Surface::Reasoning,
    );
    entry.expandable = true;
    entry.default_open = default_open;
    entry
}

/// Length-prefixed native IDs avoid collisions even when IDs contain separators.
fn response_block_key(request: u64, item: &str, block: &str) -> String {
    format!(
        "response{request}/{}:{item}/{}:{block}",
        item.len(),
        block.len()
    )
}

fn reasoning_key(request: u64, item: &str, block: &str) -> String {
    format!("reasoning-{}", response_block_key(request, item, block))
}

fn response_entries(
    request: u64,
    response: &LiveResponse,
    view: &View,
    thinking: bool,
    agent_name: &str,
) -> Vec<Entry> {
    let mut entries = Vec::new();
    for item in response.snapshot().items {
        for block in item.blocks {
            match block.kind {
                BlockKind::Reasoning if !block.text.trim().is_empty() => {
                    // A block ends independently of its item and of answer text.
                    let running = !response.settled && !block.ended;
                    let mut entry = reasoning_entry(
                        reasoning_key(request, &item.id, &block.id),
                        &block.text,
                        view,
                        running || thinking,
                        if running {
                            "  Reasoning"
                        } else if response.error.is_some() {
                            "Reasoning · incomplete"
                        } else {
                            "Reasoning"
                        },
                    );
                    entry.running = running;
                    entry.default_open = running || thinking;
                    entries.push(entry);
                }
                BlockKind::Text if !block.text.trim().is_empty() => {
                    entries.push(Entry::new(
                        response_block_key(request, &item.id, &block.id),
                        format!(
                            "{}\n{}",
                            if response.error.is_some() {
                                "Incomplete response"
                            } else {
                                agent_name
                            },
                            block.text,
                        ),
                        if response.error.is_some() {
                            Surface::Error
                        } else {
                            Surface::Agent
                        },
                    ));
                }
                _ => {}
            }
        }
    }
    entries
}

pub fn target_suffix(target: &str) -> String {
    if target == "root" {
        String::new()
    } else {
        format!(" @{target}")
    }
}

fn request_entry(
    sequence: u64,
    purpose: &ModelPurpose,
    info: &RequestInfo,
    running: bool,
) -> Entry {
    let status = if info.failed {
        "Failed"
    } else if running {
        "Running"
    } else if info.response.is_some() || info.usage.is_some() {
        "Completed"
    } else {
        "Interrupted"
    };
    let row = RequestRow {
        sequence,
        purpose: *purpose,
        model: clean(info.model.as_deref().unwrap_or("Unknown model")).replace('\n', " "),
        status,
        usage: info.usage,
        elapsed_tenths: request_elapsed(info, running),
    };
    let mut e = Entry::new(
        format!("r{sequence}"),
        row.metadata().join(" · "),
        Surface::Tool,
    );
    e.request = Some(row);
    e.running = running;
    e
}

fn request_running(
    info: &RequestInfo,
    snapshot: &ObservationSnapshot,
    projection: &Projection,
    agent: &AgentId,
    request: u64,
) -> bool {
    info.finished_millis.is_none()
        && projection.active_request.get(agent) == Some(&request)
        && snapshot
            .responses
            .get(&(agent.clone(), request))
            .is_none_or(|response| !response.settled)
        && matches!(snapshot.activity.get(agent), Some(AgentActivity::Working))
}

fn request_elapsed(info: &RequestInfo, running: bool) -> Option<u64> {
    let start = info.started_millis?;
    let end = info.finished_millis.or_else(|| {
        running.then(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(start, |duration| {
                    duration.as_millis().min(i64::MAX as u128) as i64
                })
        })
    })?;
    Some(end.saturating_sub(start).max(0) as u64 / 100)
}

/// Show the event where the model received it, using its historical payload
/// rather than the job's latest output (the same job may have since resumed).
/// Both runtime envelopes describe historical job notifications, not user text.
/// Keep recognizing the original envelopes when projecting saved sessions.
#[derive(Clone, Copy)]
enum JobNotificationKind {
    State,
    AgentMessage,
}

impl JobNotificationKind {
    fn tags(self) -> (&'static str, &'static str) {
        match self {
            Self::State => ("<skyhook_job_events>", "</skyhook_job_events>"),
            Self::AgentMessage => ("<skyhook_agent_messages>", "</skyhook_agent_messages>"),
        }
    }
}

fn job_notification_kind(text: &str) -> Option<JobNotificationKind> {
    [
        JobNotificationKind::State,
        JobNotificationKind::AgentMessage,
    ]
    .into_iter()
    .find(|kind| text.trim_start().starts_with(kind.tags().0))
}

fn job_event_entries(
    key: &str,
    text: &str,
    projection: &Projection,
    view: &View,
    all: bool,
) -> Vec<Entry> {
    let kind = job_notification_kind(text);
    let legacy_agent_messages = matches!(kind, Some(JobNotificationKind::AgentMessage));
    let events = kind.and_then(|kind| {
        let (start, end) = kind.tags();
        text.trim()
            .strip_prefix(start)
            .and_then(|text| text.strip_suffix(end))
            .and_then(|json| serde_json::from_str::<Vec<Value>>(json).ok())
    });
    let Some(events) = events.filter(|events| !events.is_empty()) else {
        return vec![Entry::new(
            format!("{key}/events"),
            "Job event · notification received by model · details unavailable".into(),
            Surface::Tool,
        )];
    };
    events
        .iter()
        .enumerate()
        .map(|(index, event)| {
            // Unified envelopes mix lifecycle and message items. The old envelope
            // remains message-only for historical sessions without a kind field.
            let agent_message = legacy_agent_messages
                || event.get("kind").and_then(Value::as_str) == Some("message");
            let key = format!("{key}/event{index}");
            let open = view.is_expanded(&key, all);
            let id = event
                .get("id")
                .and_then(Value::as_u64)
                .and_then(|id| JobId::new(id).ok());
            let job = id.and_then(|id| projection.jobs.get(&id));
            let tool = event
                .get("tool")
                .and_then(Value::as_str)
                .or_else(|| job.map(|job| job.tool.as_str()))
                .unwrap_or(if agent_message { "agent" } else { "job" });
            let state = if agent_message {
                event
                    .get("message")
                    .and_then(Value::as_u64)
                    .map_or_else(|| "message".into(), |message| format!("message #{message}"))
            } else {
                event
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or("updated")
                    .into()
            };
            let name = if agent_message {
                event
                    .get("name")
                    .and_then(Value::as_str)
                    .or_else(|| job.and_then(|job| job.name.as_deref()))
                    .map(|name| format!(" · {}", brief(&clean(name), 80)))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let mut header = vec![
                Run::new(if open { "▾" } else { "▸" }, Role::Indicator),
                Run::new(" Job event", Role::Plain),
                Run::new(" · ", Role::Muted),
                Run::new(tool, Role::ToolName),
                Run::new(
                    id.map_or(String::new(), |id| format!(" #{id}")),
                    Role::Muted,
                ),
            ];
            if let Some(name) = name.strip_prefix(" · ") {
                header.push(Run::new(" · ", Role::Muted));
                header.push(Run::new(name, Role::Plain));
            }
            header.push(Run::new(" · ", Role::Muted));
            // Only the envelope's typed state is semantic, never message prose or
            // the job's current state (this notification is historical).
            let role = if agent_message {
                Role::Plain
            } else {
                serde_json::from_value::<JobState>(event["state"].clone())
                    .map(state_role)
                    .unwrap_or(Role::Plain)
            };
            header.push(Run::new(state, role));
            let mut entry = Entry::new(key, header_text(&header), Surface::Tool);
            entry.header = Some(header.clone());
            entry.expandable = true;
            // Deliberately not Entry.job: output refresh must not replace this
            // historical notification with a live job card or discard its key.
            if open {
                let mut body = Document::default();
                body.sections.push(Section::Line(header));
                if agent_message {
                    body.line("Agent message received by model", Role::Muted);
                    if let Some(text) = event.get("text").and_then(Value::as_str) {
                        body.line(clean(text), Role::Plain);
                    } else {
                        body.line("Message text unavailable", Role::Muted);
                    }
                } else {
                    body.line("Notification received by model", Role::Muted);
                    body.output(tool, job.map_or(&Value::Null, |job| &job.args), event);
                }
                entry.text = body.plain_text();
                entry.document = Some(body);
            }
            entry
        })
        .collect()
}

/// A call without an admitted job uses the original response expansion key and
/// never acquires job navigation. The result is presentation-only session data.
#[allow(clippy::too_many_arguments)]
fn call_entry(
    key: String,
    tool: &str,
    args: Option<&Value>,
    result: Option<&ToolResult>,
    agent: &AgentId,
    projection: &Projection,
    view: &View,
    all: bool,
) -> Entry {
    let open = view.is_expanded(&key, all);
    let mut header = vec![
        Run::new(if open { "▾" } else { "▸" }, Role::Indicator),
        Run::new(" ", Role::Plain),
    ];
    if let Some(result) = result {
        header.push(Run::new(
            if result.is_error { "×" } else { "✓" },
            if result.is_error {
                Role::Error
            } else {
                Role::Success
            },
        ));
        header.push(Run::new(" ", Role::Plain));
    }
    header.push(Run::new(tool, Role::ToolName));
    if tool == "agent"
        && let Some(args) = args
    {
        header.push(Run::new(
            target_suffix(projection.child_target(agent, args)),
            Role::Target,
        ));
    }
    if let Some(result) = result {
        header.push(Run::new(" · ", Role::Muted));
        header.push(Run::new(
            if result.is_error {
                "Failed"
            } else {
                "Completed"
            },
            if result.is_error {
                Role::Error
            } else {
                Role::Success
            },
        ));
    }
    let mut entry = Entry::new(key, header_text(&header), Surface::Tool);
    entry.expandable = true;
    entry.header = Some(header.clone());
    if open {
        let mut document = Document::default();
        document.sections.push(Section::Line(header));
        if let Some(args) = args {
            document.arguments(tool, args);
        }
        if let Some(result) = result {
            document.output(tool, args.unwrap_or(&Value::Null), &result.result);
        }
        entry.text = document.plain_text();
        entry.document = Some(document);
    }
    entry
}

fn job_entry(
    job: &JobInfo,
    projection: &Projection,
    view: &View,
    outputs: &HashMap<JobId, Value>,
    all: bool,
) -> Entry {
    let key = format!("j{}", job.id);
    let open = view.is_expanded(&key, all);
    let detail = match job.tool.as_str() {
        "exec" => job
            .args
            .get("argv")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default(),
        "shell" => job.args["command"].as_str().unwrap_or_default().into(),
        "script" => "JavaScript workflow".into(),
        _ => job
            .args
            .get("path")
            .or_else(|| job.args.get("pattern"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
    };
    let symbol = match job.state {
        JobState::Completed => "✓",
        JobState::Failed => "×",
        JobState::AwaitingApproval => "◇",
        JobState::WaitingInput => "?",
        JobState::Running => "●",
        _ => "·",
    };
    let header = vec![
        Run::new(if open { "▾" } else { "▸" }, Role::Indicator),
        Run::new(" ", Role::Plain),
        Run::new(symbol, state_role(job.state)),
        Run::new(" ", Role::Plain),
        Run::new(job.tool.clone(), Role::ToolName),
        Run::new(target_suffix(projection.job_target(job)), Role::Target),
        Run::new(format!(" {}", brief(&detail, 90)), Role::Plain),
        Run::new(" · ", Role::Muted),
        Run::new(state_name(job.state), state_role(job.state)),
        Run::new(" · ", Role::Muted),
        Run::new(format!("#{}", job.id), Role::Muted),
    ];
    let mut text = header_text(&header);
    let mut document = None;
    if open {
        let mut body = Document::default();
        body.sections.push(Section::Line(header.clone()));
        body.line(job.location.clone(), Role::Muted);
        body.arguments(&job.tool, &job.args);
        if outputs.contains_key(&job.id) || job.error.is_some() {
            body.output_with_error(
                &job.tool,
                &job.args,
                outputs.get(&job.id),
                job.error.as_deref(),
            );
        } else if job.remote && !job.state.is_terminal() {
            body.line(
                "Running remotely · output available after completion",
                Role::Muted,
            );
        } else {
            body.line("Loading output…", Role::Muted);
        }
        body.line(
            "[o] output fields / search / next page    [c] cancel job",
            Role::Muted,
        );
        text = body.plain_text();
        document = Some(body);
    }
    let mut entry = Entry::new(key, text, Surface::Tool);
    entry.document = document;
    entry.header = Some(header);
    entry.expandable = true;
    entry.job = Some(job.id);
    let mut parent = job.parent;
    while let Some(p) = parent.and_then(|id| projection.jobs.get(&id)) {
        entry.indent = entry.indent.saturating_add(2).min(16);
        parent = p.parent;
    }
    entry
}
#[cfg(test)]
mod tests {
    use super::*;
    use skyhook::{
        agent::{ObservedEvent, RuntimeEvent},
        identity::SessionId,
        provider::protocol::{
            AssistantItem, BlockContent, BlockKind, ContentDelta, ItemKind, ModelRequest,
            ReplayEnvelope, ResponseEvent,
        },
        session::{ContextMessage, EventRecord},
    };
    #[test]
    fn job_headers_preserve_text_and_semantics_for_all_states_and_themes() {
        use super::super::{theme::ContentTheme, tool_view::header_line};
        use ratatui::style::Modifier;

        let projection = Projection::default();
        let states = [
            (JobState::Queued, "·", "Queued", Role::Muted),
            (
                JobState::AwaitingApproval,
                "◇",
                "Waiting for permission",
                Role::Warning,
            ),
            (JobState::Running, "●", "Running", Role::Indicator),
            (
                JobState::WaitingInput,
                "?",
                "Waiting for input",
                Role::Warning,
            ),
            (JobState::Completed, "✓", "Completed", Role::Success),
            (JobState::Failed, "×", "Failed", Role::Error),
            (JobState::Cancelled, "·", "Cancelled", Role::Muted),
            (JobState::Interrupted, "·", "Interrupted", Role::Muted),
        ];
        for (state, symbol, name, role) in states {
            for target in ["root", "build-host"] {
                let job = JobInfo {
                    id: JobId::new(42).unwrap(),
                    agent: AgentId::root(SessionId::from_bytes([1; 16])),
                    name: None,
                    tool: "exec".into(),
                    args: serde_json::json!({"argv": ["echo", "Failed @fake Completed"]}),
                    parent: None,
                    state,
                    target: target.into(),
                    location: "/workspace".into(),
                    remote: false,
                    error: Some("Failure details\nsecond line".into()),
                };
                let collapsed =
                    job_entry(&job, &projection, &View::default(), &HashMap::new(), false);
                let expanded =
                    job_entry(&job, &projection, &View::default(), &HashMap::new(), true);
                for (entry, arrow) in [(&collapsed, "▸"), (&expanded, "▾")] {
                    let expected = format!(
                        "{arrow} {symbol} exec{} echo Failed @fake Completed · {name} · #42",
                        target_suffix(target)
                    );
                    let runs = entry.header.as_ref().unwrap();
                    assert_eq!(header_text(runs), expected);
                    assert_eq!(entry.text.lines().next().unwrap(), expected);
                    assert_eq!(runs[2], Run::new(symbol, role));
                    assert_eq!(runs[8], Run::new(name, role));
                    for light in [false, true] {
                        let theme = ContentTheme::new(light);
                        let line = header_line(runs, light);
                        let spans = &line.spans;
                        let status_color = match role {
                            Role::Indicator => theme.primary,
                            Role::Warning => theme.warning,
                            Role::Success => theme.success,
                            Role::Error => theme.error,
                            _ => theme.muted,
                        };
                        assert_eq!(line.to_string(), expected);
                        assert_eq!(line.style.fg, None);
                        assert_eq!(spans[0].style.fg, Some(theme.primary));
                        assert_eq!(spans[2].style.fg, Some(status_color));
                        assert_eq!(spans[4].style.fg, Some(theme.fg));
                        assert!(spans[4].style.add_modifier.contains(Modifier::BOLD));
                        assert_eq!(spans[5].style.fg, Some(theme.accent));
                        assert_eq!(spans[6].style.fg, Some(theme.fg));
                        assert!(!spans[6].style.add_modifier.contains(Modifier::BOLD));
                        assert_eq!(spans[8].style.fg, Some(status_color));
                        for index in [7, 9, 10] {
                            assert_eq!(spans[index].style.fg, Some(theme.muted));
                        }
                        if let Some(document) = &entry.document {
                            assert_eq!(document.sections[0], Section::Line(runs.clone()));
                            let lines = document.lines(None, light);
                            assert_eq!(lines[0], line);
                            let output = lines
                                .iter()
                                .position(|line| line.to_string() == "Output")
                                .unwrap();
                            for error in &lines[output + 1..=output + 2] {
                                let text = error.spans.last().unwrap();
                                assert_eq!(text.style.fg, Some(theme.error));
                                assert!(!text.style.add_modifier.contains(Modifier::BOLD));
                            }
                        }
                    }
                }
                assert_eq!(
                    collapsed.text,
                    header_text(collapsed.header.as_ref().unwrap())
                );
                assert!(collapsed.document.is_none());
                let document = expanded.document.as_ref().unwrap();
                assert_eq!(expanded.text, document.plain_text());
                assert!(expanded.text.contains("Arguments"));
                assert!(
                    expanded
                        .text
                        .contains("Output\n  Failure details\n  second line")
                );
                assert!(!expanded.text.contains("Loading output"));
                assert_eq!(
                    collapsed.header.as_ref().unwrap()[1..],
                    expanded.header.as_ref().unwrap()[1..]
                );
            }
        }
    }

    fn call_record(snapshot: &mut ObservationSnapshot, agent: &AgentId, id: &str) -> u64 {
        record(
            snapshot,
            agent,
            SessionEvent::MessageCommitted {
                message: Message::Assistant(vec![AssistantItem::tool_call(
                    "tool",
                    0,
                    skyhook::provider::protocol::ToolCall {
                        id: id.into(),
                        name: "exec".into(),
                        arguments: serde_json::json!({"argv": ["echo", "  original\ttext\n"]}),
                    },
                )]),
            },
        )
    }

    fn result_record(snapshot: &mut ObservationSnapshot, agent: &AgentId, id: &str, error: bool) {
        record(
            snapshot,
            agent,
            SessionEvent::MessageCommitted {
                message: Message::Tool(vec![ToolResult {
                    call_id: id.into(),
                    name: "exec".into(),
                    result: if error {
                        serde_json::json!({"error": "Permission was denied", "code": "permission_denied", "executed": false})
                    } else {
                        serde_json::json!({"stdout": "  original\ttext\n"})
                    },
                    images: vec![],
                    is_error: error,
                }]),
            },
        );
    }

    #[test]
    fn synchronous_failure_refreshes_the_existing_call_after_interleaved_user_input() {
        let root = AgentId::root(SessionId::from_bytes([71; 16]));
        let mut snapshot = ObservationSnapshot::default();
        call_record(&mut snapshot, &root, "call");
        let mut projection = Projection::default();
        projection.rebuild(&snapshot);
        let mut cache = ContentCache::default();
        let mut cards = vec![];
        let outputs = HashMap::new();
        let mut view = View::default();
        cache.update(
            &mut cards,
            &snapshot,
            &projection,
            EntryView {
                agent: &root,
                view: &view,
                thinking: false,
                all_details: false,
            },
            &outputs,
            0,
        );
        assert_eq!(cards.len(), 1);
        let key = cards[0].key.clone();
        assert!(!cards[0].text.contains("Failed"));
        record(
            &mut snapshot,
            &root,
            SessionEvent::MessageCommitted {
                message: Message::User(vec![UserContent::Text {
                    text: "Continue after permission".into(),
                }]),
            },
        );
        result_record(&mut snapshot, &root, "call", true);
        let records_before = serde_json::to_value(&snapshot.records).unwrap();
        projection.rebuild(&snapshot);
        let changes = cache.update(
            &mut cards,
            &snapshot,
            &projection,
            EntryView {
                agent: &root,
                view: &view,
                thinking: false,
                all_details: false,
            },
            &outputs,
            0,
        );
        assert!(!changes.reset);
        assert!(changes.dirty.contains(&0));
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].key, key);
        assert_eq!(cards[0].surface, Surface::Tool);
        assert!(cards[0].job.is_none());
        assert_eq!(cards[0].text, "▸ × exec · Failed");
        assert!(!cards[0].text.contains("Permission"));
        view.expanded.insert(key.clone());
        cache.update(
            &mut cards,
            &snapshot,
            &projection,
            EntryView {
                agent: &root,
                view: &view,
                thinking: false,
                all_details: false,
            },
            &outputs,
            1,
        );
        assert_eq!(cards[0].key, key);
        assert!(cards[0].text.contains("Arguments"));
        assert!(cards[0].text.contains("Output\n  Permission was denied"));
        assert!(cards[0].text.contains("permission_denied"));
        assert_eq!(cards[0].text.matches("Permission was denied").count(), 1);
        assert_eq!(
            records_before,
            serde_json::to_value(&snapshot.records).unwrap()
        );
    }

    #[test]
    fn synchronous_results_are_turn_scoped_and_orphans_remain_expandable() {
        let root = AgentId::root(SessionId::from_bytes([72; 16]));
        let mut snapshot = ObservationSnapshot::default();
        call_record(&mut snapshot, &root, "reused");
        result_record(&mut snapshot, &root, "reused", false);
        call_record(&mut snapshot, &root, "reused");
        result_record(&mut snapshot, &root, "reused", true);
        result_record(&mut snapshot, &root, "orphan", true);
        let mut projection = Projection::default();
        projection.rebuild(&snapshot);
        let cards = entries(
            &snapshot,
            &projection,
            &root,
            &View::default(),
            &HashMap::new(),
            false,
            true,
        );
        assert_eq!(cards.len(), 3);
        assert!(cards[0].text.starts_with("▾ ✓ exec · Completed"));
        assert!(cards[1].text.starts_with("▾ × exec · Failed"));
        assert_ne!(cards[0].key, cards[1].key);
        assert!(cards[2].text.starts_with("▾ × exec · Failed"));
        assert!(!cards[2].text.contains("Arguments"));
        assert!(cards[2].text.contains("Output"));
        assert!(cards[2].text.contains("permission_denied"));
        assert!(
            cards
                .iter()
                .all(|card| card.expandable && card.job.is_none() && card.surface == Surface::Tool)
        );

        // A new assistant turn closes the old call scope even if its old call
        // never produced a result. Same-ID results in other agents cannot bind.
        let other = root.child(1);
        result_record(&mut snapshot, &other, "reused", true);
        call_record(&mut snapshot, &root, "pending");
        call_record(&mut snapshot, &root, "new-turn");
        result_record(&mut snapshot, &root, "pending", true);
        projection.rebuild(&snapshot);
        let cards = entries(
            &snapshot,
            &projection,
            &root,
            &View::default(),
            &HashMap::new(),
            false,
            false,
        );
        assert_eq!(cards.len(), 6);
        assert_eq!(cards[3].text, "▸ exec");
        assert_eq!(cards[4].text, "▸ exec");
        assert_eq!(cards[5].text, "▸ × exec · Failed");
        let other_cards = entries(
            &snapshot,
            &projection,
            &other,
            &View::default(),
            &HashMap::new(),
            false,
            false,
        );
        assert_eq!(other_cards.len(), 1);
    }

    #[test]
    fn admitted_results_use_exact_job_provenance_even_without_retained_calls() {
        use skyhook::{execution::ExecutionLocation, session::ModelCallOrigin};
        for retained in [false, true] {
            let root = AgentId::root(SessionId::from_bytes([73; 16]));
            let mut snapshot = ObservationSnapshot::default();
            let origin = call_record(&mut snapshot, &root, "reused");
            record(
                &mut snapshot,
                &root,
                SessionEvent::JobCreated {
                    job: JobId::new(42).unwrap(),
                    parent: None,
                    origin: Some(ModelCallOrigin {
                        message: origin,
                        call_id: "reused".into(),
                    }),
                    tool: "exec".into(),
                    name: None,
                    arguments: serde_json::json!({"argv": ["echo"]}),
                    output_schema: None,
                    accepts_input: false,
                    background: false,
                    location: ExecutionLocation::root("/workspace".into()),
                },
            );
            result_record(&mut snapshot, &root, "reused", false);
            if !retained {
                snapshot.records.remove(&origin);
            }
            // A later call reuses the ID but fails before creating a job.
            call_record(&mut snapshot, &root, "reused");
            result_record(&mut snapshot, &root, "reused", true);
            let mut projection = Projection::default();
            projection.rebuild(&snapshot);
            let cards = entries(
                &snapshot,
                &projection,
                &root,
                &View::default(),
                &HashMap::new(),
                false,
                false,
            );
            assert_eq!(cards.len(), 2);
            assert_eq!(cards[0].job, Some(JobId::new(42).unwrap()));
            assert!(cards[1].job.is_none());
            assert_eq!(cards[1].text, "▸ × exec · Failed");
        }
    }

    #[test]
    fn failed_jobs_put_errors_in_expanded_output_and_keep_real_results() {
        let id = JobId::new(42).unwrap();
        let job = JobInfo {
            id,
            agent: AgentId::root(SessionId::from_bytes([74; 16])),
            name: None,
            tool: "exec".into(),
            args: serde_json::json!({"argv": ["echo"]}),
            parent: None,
            state: JobState::Failed,
            target: "root".into(),
            location: "/workspace".into(),
            remote: false,
            error: Some("failed exactly".into()),
        };
        let projection = Projection::default();
        for output in [
            None,
            Some(
                serde_json::json!({"error": "failed exactly", "result": {"stdout": "  saved output\t\n"}}),
            ),
            Some(serde_json::json!({"result": {"stderr": "other details"}})),
        ] {
            let outputs: HashMap<_, _> = output.into_iter().map(|output| (id, output)).collect();
            let collapsed = job_entry(&job, &projection, &View::default(), &outputs, false);
            assert_eq!(collapsed.text.lines().count(), 1);
            assert!(!collapsed.text.contains("failed exactly"));
            assert!(collapsed.document.is_none());
            let expanded = job_entry(&job, &projection, &View::default(), &outputs, true);
            assert!(expanded.text.contains("Output\n  failed exactly"));
            assert_eq!(expanded.text.matches("failed exactly").count(), 1);
            assert!(!expanded.text.contains("Loading output"));
            if let Some(stdout) = outputs
                .get(&id)
                .and_then(|value| value.pointer("/result/stdout"))
            {
                assert!(expanded.document.as_ref().unwrap().sections.iter().any(|section| {
                    matches!(section, Section::Code { source, .. } if &**source == stdout.as_str().unwrap())
                }));
            }
            if outputs
                .get(&id)
                .is_some_and(|value| value.pointer("/result/stderr").is_some())
            {
                assert!(expanded.text.contains("other details"));
            }
        }
    }

    #[test]
    fn pending_tool_calls_share_structured_headers_with_expanded_documents() {
        use super::super::tool_view::header_line;
        let mut snapshot = ObservationSnapshot::default();
        let root = AgentId::root(SessionId::from_bytes([3; 16]));
        let context = context(&mut snapshot, &root);
        request(&mut snapshot, &root, context);
        record(
            &mut snapshot,
            &root,
            SessionEvent::MessageCommitted {
                message: Message::Assistant(vec![AssistantItem::tool_call(
                    "tool",
                    0,
                    skyhook::provider::protocol::ToolCall {
                        id: "call_agent".into(),
                        name: "agent".into(),
                        arguments: serde_json::json!({"target":"build-host"}),
                    },
                )]),
            },
        );
        let mut projection = Projection::default();
        projection.rebuild(&snapshot);
        for open in [false, true] {
            let cards = entries(
                &snapshot,
                &projection,
                &root,
                &View::default(),
                &HashMap::new(),
                false,
                open,
            );
            let card = cards
                .iter()
                .find(|card| card.surface == Surface::Tool)
                .unwrap();
            let header = card.header.as_ref().unwrap();
            assert_eq!(
                header,
                &vec![
                    Run::new(if open { "▾" } else { "▸" }, Role::Indicator),
                    Run::new(" ", Role::Plain),
                    Run::new("agent", Role::ToolName),
                    Run::new(" @build-host", Role::Target),
                ]
            );
            assert_eq!(
                card.text.lines().next().unwrap(),
                if open {
                    "▾ agent @build-host"
                } else {
                    "▸ agent @build-host"
                }
            );
            for light in [false, true] {
                if open {
                    assert_eq!(
                        card.document.as_ref().unwrap().lines(None, light)[0],
                        header_line(header, light)
                    );
                }
            }
        }
    }

    #[test]
    fn notification_headers_use_only_historical_typed_states() {
        let text = format!(
            "<skyhook_job_events>{}</skyhook_job_events>",
            serde_json::json!([
                {"id": 1, "tool": "exec", "state": "failed"},
                {"id": 2, "tool": "exec", "state": "not failed but updated"},
                {"id": 3, "tool": "agent", "kind": "message", "name": "Failed", "message": 4, "state": "failed", "text": "Completed"}
            ])
        );
        let cards = job_event_entries("m1", &text, &Projection::default(), &View::default(), true);
        let expected = [
            ("▾ Job event · exec #1 · failed", Role::Error),
            (
                "▾ Job event · exec #2 · not failed but updated",
                Role::Plain,
            ),
            ("▾ Job event · agent #3 · Failed · message #4", Role::Plain),
        ];
        for (card, (text, role)) in cards.iter().zip(expected) {
            let runs = card.header.as_ref().unwrap();
            assert_eq!(header_text(runs), text);
            assert_eq!(
                runs.last().unwrap(),
                &Run::new(text.rsplit(" · ").next().unwrap(), role)
            );
            assert_eq!(
                card.document.as_ref().unwrap().sections[0],
                Section::Line(runs.clone())
            );
        }
    }

    #[test]
    fn content_cache_refreshes_only_invalidated_tool_output() {
        let agent = AgentId::root(SessionId::from_bytes([34; 16]));
        let snapshot = ObservationSnapshot::default();
        let mut projection = Projection::default();
        for id in 1..=2 {
            let id = JobId::new(id).unwrap();
            projection.jobs.insert(
                id,
                JobInfo {
                    id,
                    agent: agent.clone(),
                    name: None,
                    tool: "exec".into(),
                    args: serde_json::json!({"argv": ["echo", "hi"]}),
                    parent: None,
                    state: JobState::Completed,
                    target: "root".into(),
                    location: "/tmp".into(),
                    remote: false,
                    error: None,
                },
            );
        }
        let view = View {
            tab: Tab::Jobs,
            ..View::default()
        };
        let mut outputs = HashMap::new();
        let mut cache = ContentCache::default();
        let mut rows = Vec::new();
        cache.update(
            &mut rows,
            &snapshot,
            &projection,
            EntryView {
                agent: &agent,
                view: &view,
                thinking: false,
                all_details: true,
            },
            &outputs,
            0,
        );
        let job = JobId::new(1).unwrap();
        outputs.insert(
            job,
            serde_json::json!({"stdout": "new output", "exit_code": 0}),
        );
        cache.invalidate_job(job);
        let changes = cache.update(
            &mut rows,
            &snapshot,
            &projection,
            EntryView {
                agent: &agent,
                view: &view,
                thinking: false,
                all_details: true,
            },
            &outputs,
            0,
        );
        assert!(!changes.reset);
        assert_eq!(changes.dirty, vec![0]);
        assert!(rows[0].text.contains("new output"));
    }

    #[test]
    fn content_cache_reasoning_matches_uncached_across_shape_changes() {
        let agent = AgentId::root(SessionId::from_bytes([32; 16]));
        let mut snapshot = ObservationSnapshot::default();
        let context = context(&mut snapshot, &agent);
        let request = request(&mut snapshot, &agent, context);
        let mut projection = Projection::default();
        projection.rebuild(&snapshot);
        let view = View::default();
        let outputs = HashMap::new();
        let mut cache = ContentCache::default();
        let mut rows = Vec::new();
        cache.observe_response(&agent, request);
        cache.update(
            &mut rows,
            &snapshot,
            &projection,
            EntryView {
                agent: &agent,
                view: &view,
                thinking: false,
                all_details: false,
            },
            &outputs,
            0,
        );
        for text in ["\n", "first", "\n", "second", "\r\n", "third", "\n\n"] {
            update(
                &mut snapshot,
                delta_event(agent.clone(), request, BlockKind::Reasoning, text.into()),
            );
            cache.observe_response(&agent, request);
            cache.update(
                &mut rows,
                &snapshot,
                &projection,
                EntryView {
                    agent: &agent,
                    view: &view,
                    thinking: false,
                    all_details: false,
                },
                &outputs,
                0,
            );
            let expected = entries(
                &snapshot,
                &projection,
                &agent,
                &view,
                &outputs,
                false,
                false,
            );
            assert_eq!(
                rows.iter()
                    .map(|r| (&r.key, &r.text, r.expandable, r.running))
                    .collect::<Vec<_>>(),
                expected
                    .iter()
                    .map(|r| (&r.key, &r.text, r.expandable, r.running))
                    .collect::<Vec<_>>()
            );
        }
        update(
            &mut snapshot,
            delta_event(agent.clone(), request, BlockKind::Text, "answer".into()),
        );
        cache.observe_response(&agent, request);
        cache.update(
            &mut rows,
            &snapshot,
            &projection,
            EntryView {
                agent: &agent,
                view: &view,
                thinking: false,
                all_details: false,
            },
            &outputs,
            0,
        );
        let expected = entries(
            &snapshot,
            &projection,
            &agent,
            &view,
            &outputs,
            false,
            false,
        );
        assert_eq!(
            rows.iter()
                .map(|r| (&r.key, &r.text, r.expandable, r.running))
                .collect::<Vec<_>>(),
            expected
                .iter()
                .map(|r| (&r.key, &r.text, r.expandable, r.running))
                .collect::<Vec<_>>()
        );
        // Equal-length authoritative replacement and block-only closure must invalidate cards.
        let provisional = snapshot.responses[&(agent.clone(), request)]
            .snapshot()
            .items[0]
            .blocks[0]
            .text
            .clone();
        let replacement = provisional.replace("first", "FIRST");
        assert_eq!(replacement.len(), provisional.len());
        response_event(
            &mut snapshot,
            &agent,
            request,
            ResponseEvent::BlockEnded {
                item: "reasoning".into(),
                block: "reasoning:0".into(),
                content: BlockContent::Reasoning { text: replacement },
            },
        );
        response_event(
            &mut snapshot,
            &agent,
            request,
            ResponseEvent::ItemEnded {
                id: "reasoning".into(),
                replay: Some(replay()),
            },
        );
        cache.observe_response(&agent, request);
        cache.update(
            &mut rows,
            &snapshot,
            &projection,
            EntryView {
                agent: &agent,
                view: &view,
                thinking: false,
                all_details: false,
            },
            &outputs,
            0,
        );
        let expected = entries(
            &snapshot,
            &projection,
            &agent,
            &view,
            &outputs,
            false,
            false,
        );
        assert!(rows == expected);
        assert!(
            rows.iter()
                .filter(|entry| entry.surface == Surface::Reasoning)
                .all(|entry| !entry.running)
        );
    }

    #[test]
    fn requests_remain_metadata_only_during_streaming_and_after_completion() {
        let agent = AgentId::root(SessionId::from_bytes([33; 16]));
        let mut snapshot = ObservationSnapshot::default();
        let context = context(&mut snapshot, &agent);
        let request = request(&mut snapshot, &agent, context);
        let mut projection = Projection::default();
        projection.rebuild(&snapshot);
        let mut view = View {
            tab: Tab::Requests,
            ..View::default()
        };
        view.expanded.insert(format!("r{request}"));
        let outputs = HashMap::new();
        let mut cache = ContentCache::default();
        let mut rows = Vec::new();
        cache.observe_response(&agent, request);
        cache.update(
            &mut rows,
            &snapshot,
            &projection,
            EntryView {
                agent: &agent,
                view: &view,
                thinking: false,
                all_details: false,
            },
            &outputs,
            0,
        );
        for (reasoning, text) in [
            (true, "reason"),
            (false, "answer"),
            (true, " more"),
            (false, " more"),
        ] {
            let event = if reasoning {
                delta_event(agent.clone(), request, BlockKind::Reasoning, text.into())
            } else {
                delta_event(agent.clone(), request, BlockKind::Text, text.into())
            };
            update(&mut snapshot, event);
            cache.observe_response(&agent, request);
            cache.update(
                &mut rows,
                &snapshot,
                &projection,
                EntryView {
                    agent: &agent,
                    view: &view,
                    thinking: false,
                    all_details: false,
                },
                &outputs,
                0,
            );
            let expected = entries(
                &snapshot,
                &projection,
                &agent,
                &view,
                &outputs,
                false,
                false,
            );
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].text, expected[0].text);
            assert!(!rows[0].expandable);
            assert!(!rows[0].text.contains('\n'));
            assert!(!rows[0].text.contains("original request"));
            assert!(!rows[0].text.contains("answer"));
            assert!(!rows[0].text.contains("reason"));
            assert!(rows[0].request.as_ref().unwrap().usage.is_none());
        }
        record(
            &mut snapshot,
            &agent,
            SessionEvent::Usage {
                request: Some(request),
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    cached_input_tokens: 80,
                },
            },
        );
        record(
            &mut snapshot,
            &agent,
            SessionEvent::MessageCommitted {
                message: Message::Assistant(vec![AssistantItem::text(
                    "saved",
                    0,
                    "saved response",
                )]),
            },
        );
        record(
            &mut snapshot,
            &agent,
            SessionEvent::ModelRequested {
                context,
                messages: vec![],
                purpose: ModelPurpose::Compaction,
            },
        );
        projection.rebuild(&snapshot);
        cache.update(
            &mut rows,
            &snapshot,
            &projection,
            EntryView {
                agent: &agent,
                view: &view,
                thinking: true,
                all_details: true,
            },
            &outputs,
            1,
        );
        assert_eq!(rows.len(), 2);
        assert!(rows[0].text.contains("Completed"));
        assert!(rows[1].text.contains("Compaction"));
        assert!(rows.iter().all(|row| !row.expandable
            && !row.text.contains('\n')
            && !row.text.contains("saved response")));
        assert_eq!(
            rows[0].request.as_ref().unwrap().usage,
            Some(Usage {
                input_tokens: 100,
                output_tokens: 20,
                cached_input_tokens: 80,
            })
        );
    }

    fn replay() -> ReplayEnvelope {
        ReplayEnvelope {
            version: 1,
            protocol: "fixture".into(),
            model: "fixture".into(),
            scope: "reasoning".into(),
            payload: serde_json::json!({"signature": "opaque"}),
        }
    }

    fn delta_event(agent: AgentId, request: u64, kind: BlockKind, text: String) -> RuntimeEvent {
        let item = match kind {
            BlockKind::Text => "text",
            BlockKind::Reasoning => "reasoning",
            _ => unreachable!(),
        };
        RuntimeEvent::ResponseEvent {
            agent,
            request,
            event: ResponseEvent::BlockDelta {
                item: item.into(),
                block: format!("{item}:0"),
                delta: ContentDelta::Text(text),
            },
        }
    }

    fn response_event(
        snapshot: &mut ObservationSnapshot,
        agent: &AgentId,
        request: u64,
        event: ResponseEvent,
    ) {
        update(
            snapshot,
            RuntimeEvent::ResponseEvent {
                agent: agent.clone(),
                request,
                event,
            },
        );
    }

    fn update(snapshot: &mut ObservationSnapshot, event: RuntimeEvent) {
        // Delta fixtures start their native item/block once; production receives only full protocol events.
        if let RuntimeEvent::ResponseEvent {
            agent,
            request,
            event: ResponseEvent::BlockDelta { item, block, .. },
        } = &event
        {
            let exists = snapshot
                .responses
                .get(&(agent.clone(), *request))
                .is_some_and(|live| live.snapshot().items.iter().any(|entry| entry.id == *item));
            if !exists {
                let (position, kind, block_kind) = if item == "reasoning" {
                    (0, ItemKind::Reasoning, BlockKind::Reasoning)
                } else {
                    (1, ItemKind::Text, BlockKind::Text)
                };
                response_event(
                    snapshot,
                    agent,
                    *request,
                    ResponseEvent::ItemStarted {
                        id: item.clone(),
                        position,
                        kind,
                    },
                );
                response_event(
                    snapshot,
                    agent,
                    *request,
                    ResponseEvent::BlockStarted {
                        item: item.clone(),
                        id: block.clone(),
                        position: 0,
                        kind: block_kind,
                    },
                );
            }
        }
        snapshot.apply(ObservedEvent {
            revision: snapshot.revision + 1,
            event,
        });
    }
    fn record(snapshot: &mut ObservationSnapshot, agent: &AgentId, event: SessionEvent) -> u64 {
        let sequence = snapshot
            .records
            .last_key_value()
            .map_or(1, |(sequence, _)| sequence + 1);
        update(
            snapshot,
            RuntimeEvent::Record(Box::new(EventRecord {
                version: 1,
                sequence,
                timestamp_millis: 0,
                agent: agent.clone(),
                event,
            })),
        );
        sequence
    }
    fn context(snapshot: &mut ObservationSnapshot, agent: &AgentId) -> u64 {
        record(
            snapshot,
            agent,
            SessionEvent::ModelContext {
                provider: "fixture".into(),
                template: ModelRequest {
                    model: "fixture-model".into(),
                    system: vec![],
                    messages: vec![],
                    tools: vec![],
                    response_schema: None,
                    reasoning: None,
                    max_output_tokens: Some(100),
                    correlation: None,
                },
            },
        )
    }
    fn request(snapshot: &mut ObservationSnapshot, agent: &AgentId, context: u64) -> u64 {
        record(
            snapshot,
            agent,
            SessionEvent::ModelRequested {
                context,
                messages: vec![ContextMessage::Inline {
                    message: Message::User(vec![UserContent::Text {
                        text: "original request".into(),
                    }]),
                }],
                purpose: ModelPurpose::Agent,
            },
        )
    }

    #[test]
    fn reasoning_streams_separately_and_completion_collapses_without_duplicate_handoff() {
        for settle_first in [false, true] {
            let agent = AgentId::root(SessionId::from_bytes([1; 16]));
            let mut snapshot = ObservationSnapshot::default();
            let context = context(&mut snapshot, &agent);
            let request = request(&mut snapshot, &agent, context);
            let mut view = View::default();
            let rows = |snapshot: &ObservationSnapshot, view: &View| {
                let mut projection = Projection::default();
                projection.rebuild(snapshot);
                entries(
                    snapshot,
                    &projection,
                    &agent,
                    view,
                    &HashMap::new(),
                    false,
                    false,
                )
            };
            for text in ["First step", "\nSecond step"] {
                update(
                    &mut snapshot,
                    delta_event(agent.clone(), request, BlockKind::Reasoning, text.into()),
                );
            }
            let live = rows(&snapshot, &view);
            assert_eq!(live.len(), 1);
            assert_eq!(live[0].text, "▾   Reasoning\nFirst step\nSecond step");
            assert!(live[0].expandable && live[0].default_open && live[0].running);
            view.collapsed.insert(live[0].key.clone());
            update(
                &mut snapshot,
                delta_event(
                    agent.clone(),
                    request,
                    BlockKind::Reasoning,
                    "\nThird step".into(),
                ),
            );
            assert_eq!(rows(&snapshot, &view)[0].text, "▸   Reasoning");
            view.collapsed.clear();
            response_event(
                &mut snapshot,
                &agent,
                request,
                ResponseEvent::BlockEnded {
                    item: "reasoning".into(),
                    block: "reasoning:0".into(),
                    content: BlockContent::Reasoning {
                        text: "First step\nSecond step\nThird step".into(),
                    },
                },
            );
            assert!(!rows(&snapshot, &view)[0].running);
            response_event(
                &mut snapshot,
                &agent,
                request,
                ResponseEvent::ItemEnded {
                    id: "reasoning".into(),
                    replay: Some(replay()),
                },
            );
            update(
                &mut snapshot,
                delta_event(agent.clone(), request, BlockKind::Text, "Answer".into()),
            );
            let live = rows(&snapshot, &view);
            assert_eq!(live.len(), 2);
            assert_eq!(live[0].text, "▸ Reasoning");
            assert!(!live[0].running);
            assert_eq!(live[1].text, "Agent\nAnswer");
            assert_eq!(live[1].surface, Surface::Agent);

            let sequence = snapshot.records.last_key_value().unwrap().0 + 1;
            let settled = RuntimeEvent::ResponseSettled {
                agent: agent.clone(),
                request,
                message: Some(sequence),
                error: None,
            };
            if settle_first {
                update(&mut snapshot, settled.clone());
                assert_eq!(rows(&snapshot, &view)[0].text, "▸ Reasoning");
            }
            record(
                &mut snapshot,
                &agent,
                SessionEvent::MessageCommitted {
                    message: Message::Assistant(vec![
                        AssistantItem::reasoning(
                            "reasoning",
                            0,
                            "First step\nSecond step\nThird step",
                            Some(replay()),
                        ),
                        AssistantItem::text("text", 1, "Answer"),
                    ]),
                },
            );
            let completed = rows(&snapshot, &view);
            assert_eq!(completed.len(), 2, "committed and live must not duplicate");
            assert_eq!(completed[0].text, "▸ Reasoning");
            assert!(!completed[0].default_open);
            if !settle_first {
                update(&mut snapshot, settled);
            }
            view.expanded.insert(completed[0].key.clone());
            assert!(rows(&snapshot, &view)[0].text.contains("Third step"));

            let records: Vec<EventRecord> = serde_json::from_slice(
                &serde_json::to_vec(&snapshot.records.values().collect::<Vec<_>>()).unwrap(),
            )
            .unwrap();
            let mut replay = ObservationSnapshot::default();
            for record in records {
                update(&mut replay, RuntimeEvent::Record(Box::new(record)));
            }
            assert_eq!(rows(&replay, &View::default())[0].text, "▸ Reasoning");
            assert!(rows(&replay, &view)[0].text.contains("Third step"));
        }
    }

    #[test]
    fn whitespace_only_tool_turns_do_not_create_empty_agent_cards() {
        for whitespace in ["\n\n", "\n\n\n", " \t\r\n", "\u{2003}"] {
            let mut snapshot = ObservationSnapshot::default();
            let root = AgentId::root(SessionId::from_bytes([3; 16]));
            let context = context(&mut snapshot, &root);
            request(&mut snapshot, &root, context);
            let message = Message::Assistant(vec![
                AssistantItem::reasoning("reasoning", 0, "Inspect the repository.", Some(replay())),
                AssistantItem::text("separator", 1, whitespace),
                AssistantItem::tool_call(
                    "tool",
                    2,
                    skyhook::provider::protocol::ToolCall {
                        id: "call_read".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path":"."}),
                    },
                ),
            ]);
            let original = serde_json::to_vec(&message).unwrap();
            let sequence = record(
                &mut snapshot,
                &root,
                SessionEvent::MessageCommitted { message },
            );
            let mut projection = Projection::default();
            projection.rebuild(&snapshot);
            let cards = entries(
                &snapshot,
                &projection,
                &root,
                &View::default(),
                &HashMap::new(),
                false,
                true,
            );
            assert!(cards.iter().all(|card| card.surface != Surface::Agent));
            assert!(cards.iter().any(|card| card.surface == Surface::Reasoning
                && card.text.contains("Inspect the repository.")));
            assert!(
                cards
                    .iter()
                    .any(|card| card.surface == Surface::Tool && card.text.contains("read"))
            );
            let SessionEvent::MessageCommitted { message } = &snapshot.records[&sequence].event
            else {
                panic!("message history changed during rendering");
            };
            assert_eq!(serde_json::to_vec(message).unwrap(), original);
        }
    }

    #[test]
    fn live_text_waits_for_visible_content_without_trimming_the_response() {
        let mut snapshot = ObservationSnapshot::default();
        let root = AgentId::root(SessionId::from_bytes([4; 16]));
        let context = context(&mut snapshot, &root);
        let request = request(&mut snapshot, &root, context);
        update(
            &mut snapshot,
            delta_event(root.clone(), request, BlockKind::Text, "\n\n".into()),
        );
        let cards = response_entries(
            request,
            &snapshot.responses[&(root.clone(), request)],
            &View::default(),
            false,
            "Agent",
        );
        assert!(
            cards.is_empty(),
            "newline-only streaming deltas must not create a blank card"
        );
        update(
            &mut snapshot,
            delta_event(
                root.clone(),
                request,
                BlockKind::Text,
                "  Actual answer.\n".into(),
            ),
        );
        let cards = response_entries(
            request,
            &snapshot.responses[&(root, request)],
            &View::default(),
            false,
            "Agent",
        );
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].surface, Surface::Agent);
        assert_eq!(cards[0].text, "Agent\n\n\n  Actual answer.\n");
    }

    #[test]
    fn trailing_whitespace_does_not_steal_the_final_answer_footer() {
        let mut snapshot = ObservationSnapshot::default();
        let root = AgentId::root(SessionId::from_bytes([5; 16]));
        let context = context(&mut snapshot, &root);
        request(&mut snapshot, &root, context);
        let answer = "  Actual answer with spacing.\n";
        record(
            &mut snapshot,
            &root,
            SessionEvent::MessageCommitted {
                message: Message::Assistant(vec![
                    AssistantItem::text("answer", 0, answer),
                    AssistantItem::text("separator", 1, "\n\n"),
                ]),
            },
        );
        let mut projection = Projection::default();
        projection.rebuild(&snapshot);
        let cards = entries(
            &snapshot,
            &projection,
            &root,
            &View::default(),
            &HashMap::new(),
            false,
            false,
        );
        let answers: Vec<_> = cards
            .iter()
            .filter(|card| card.surface == Surface::Agent)
            .collect();
        assert_eq!(answers.len(), 1);
        assert!(answers[0].text.ends_with(answer));
        assert_eq!(answers[0].footer.as_deref(), Some("fixture-model"));
    }

    #[test]
    fn intermediate_agent_messages_are_historical_expandable_job_events() {
        let message = "Both reviewer gaps are fixed.\nChecking \"native\" replay — next.";
        let text = format!(
            " <skyhook_agent_messages>\n{}\n</skyhook_agent_messages> ",
            serde_json::json!([{
                "id":253, "name":"implement-native-replay", "message":6577, "text":message,
            }]),
        );
        let mut projection = Projection::default();
        let collapsed = job_event_entries("m42/0", &text, &projection, &View::default(), false);
        assert_eq!(collapsed.len(), 1);
        let card = &collapsed[0];
        assert_eq!(card.surface, Surface::Tool);
        assert!(card.expandable);
        assert!(card.job.is_none());
        assert_eq!(card.key, "m42/0/event0");
        assert!(
            card.text
                .contains("Job event · agent #253 · implement-native-replay · message #6577")
        );
        assert!(!card.text.contains("skyhook_agent_messages"));
        assert!(!card.text.contains(message));
        let mut view = View::default();
        view.expanded.insert(card.key.clone());
        let expanded = job_event_entries("m42/0", &text, &projection, &view, false);
        assert!(expanded[0].document.is_some());
        assert!(expanded[0].text.contains(message));
        assert!(!expanded[0].text.contains("<skyhook_"));
        assert!(!expanded[0].text.contains("\\\"text\\\""));

        // A later terminal job snapshot must not relabel or replace the historical message.
        let id = JobId::new(253).unwrap();
        projection.jobs.insert(
            id,
            JobInfo {
                id,
                agent: AgentId::root(SessionId::from_bytes([1; 16])),
                name: Some("current name".into()),
                tool: "agent".into(),
                args: Value::Null,
                parent: None,
                state: JobState::Completed,
                target: "host".into(),
                location: ".".into(),
                remote: false,
                error: None,
            },
        );
        let after_completion = job_event_entries("m42/0", &text, &projection, &view, false);
        assert_eq!(after_completion[0].text, expanded[0].text);
        assert!(after_completion[0].job.is_none());
        view.collapsed.insert(card.key.clone());
        assert!(
            job_event_entries("m42/0", &text, &projection, &view, true)[0]
                .document
                .is_none()
        );
    }

    #[test]
    fn unified_agent_message_preserves_legacy_expansion_and_attribution() {
        let payload = serde_json::json!([
            {"id":253,"name":"reviewer","message":6577,"text":"Historical reply.\nNext line."},
        ]);
        let legacy = format!("<skyhook_agent_messages>\n{payload}\n</skyhook_agent_messages>");
        let mut unified_payload = payload;
        unified_payload[0]["kind"] = serde_json::json!("message");
        let unified = format!("<skyhook_job_events>\n{unified_payload}\n</skyhook_job_events>");
        let projection = Projection::default();
        for expanded in [false, true] {
            let view = View::default();
            let legacy = job_event_entries("m42/0", &legacy, &projection, &view, expanded);
            let unified = job_event_entries("m42/0", &unified, &projection, &view, expanded);
            assert!(
                unified == legacy,
                "envelope migration changed message presentation"
            );
        }
    }

    #[test]
    fn conversation_projects_mixed_and_legacy_job_events_without_reclassifying_user_text() {
        let agent = AgentId::root(SessionId::from_bytes([2; 16]));
        let messages = format!(
            "<skyhook_agent_messages>\n{}\n</skyhook_agent_messages>",
            serde_json::json!([
                {"id":253,"message":6577,"text":"first progress"},
                {"id":253,"message":6578,"text":"second progress"},
            ]),
        );
        let jobs = format!(
            "<skyhook_job_events>\n{}\n</skyhook_job_events>",
            serde_json::json!([
                {"kind":"message","id":253,"name":"reviewer","message":6579,"text":"independent reply"},
                {"id":253,"tool":"agent","state":"completed","last_message":6579},
                {"kind":"message","id":254,"message":6580,"text":"another child reply"},
                {"id":255,"tool":"exec","state":"completed","result":{"stdout":"tool output"}},
            ]),
        );
        let message = Message::User(vec![
            UserContent::Runtime {
                text: messages.clone(),
            },
            UserContent::Runtime { text: jobs },
            UserContent::Text {
                text: messages.clone(),
            },
            UserContent::Runtime {
                text: "ordinary scheduler note".into(),
            },
        ]);
        // Saved histories retain the original runtime envelopes; rendering is presentation-only.
        let saved = serde_json::to_vec(&message).unwrap();
        let restored = serde_json::from_slice(&saved).unwrap();
        let mut snapshot = ObservationSnapshot::default();
        let sequence = record(
            &mut snapshot,
            &agent,
            SessionEvent::MessageCommitted { message: restored },
        );
        let mut projection = Projection::default();
        projection.rebuild(&snapshot);
        let cards = entries(
            &snapshot,
            &projection,
            &agent,
            &View::default(),
            &HashMap::new(),
            false,
            true,
        );
        let notifications: Vec<_> = cards
            .iter()
            .filter(|entry| entry.surface == Surface::Tool)
            .collect();
        assert_eq!(notifications.len(), 6);
        assert_eq!(notifications[0].key, format!("m{sequence}/0/event0"));
        assert_eq!(notifications[1].key, format!("m{sequence}/0/event1"));
        for (index, notification) in notifications[2..].iter().enumerate() {
            assert_eq!(notification.key, format!("m{sequence}/1/event{index}"));
        }
        assert!(notifications[0].text.contains("first progress"));
        assert!(notifications[1].text.contains("second progress"));
        assert!(
            notifications[2]
                .text
                .contains("agent #253 · reviewer · message #6579")
        );
        assert!(
            notifications[2]
                .text
                .contains("Agent message received by model")
        );
        assert!(notifications[2].text.contains("independent reply"));
        assert!(notifications[3].text.contains("agent #253 · completed"));
        assert!(
            !notifications[3]
                .text
                .contains("Agent message received by model")
        );
        assert!(!notifications[3].text.contains("independent reply"));
        assert!(notifications[4].text.contains("agent #254 · message #6580"));
        assert!(
            notifications[4]
                .text
                .contains("Agent message received by model")
        );
        assert!(notifications[4].text.contains("another child reply"));
        assert!(notifications[5].text.contains("exec #255 · completed"));
        assert!(notifications[5].text.contains("tool output"));
        assert!(
            !notifications[5]
                .text
                .contains("Agent message received by model")
        );
        assert!(notifications.iter().all(|entry| entry.expandable
            && entry.job.is_none()
            && !entry.text.contains("<skyhook_")
            && !entry.text.contains("Harness notification")));
        assert!(notifications[0].compact_after);
        assert!(!notifications[1].compact_after);
        assert!(cards.iter().any(
            |entry| entry.surface == Surface::User && entry.text == format!("You\n{messages}")
        ));
        assert!(cards.iter().any(|entry| entry.surface == Surface::Muted
            && entry.text == "Harness notification\nordinary scheduler note"));
        let SessionEvent::MessageCommitted { message: stored } = &snapshot.records[&sequence].event
        else {
            panic!("message history changed during rendering");
        };
        assert_eq!(serde_json::to_vec(stored).unwrap(), saved);
    }

    #[test]
    fn malformed_agent_notifications_use_job_event_fallback_without_panicking() {
        for text in [
            "<skyhook_agent_messages>bad json</skyhook_agent_messages>",
            "<skyhook_agent_messages>[]</skyhook_agent_messages>",
            "<skyhook_agent_messages>[{}]",
        ] {
            assert!(job_notification_kind(text).is_some());
            let entries =
                job_event_entries("m1/0", text, &Projection::default(), &View::default(), true);
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].surface, Surface::Tool);
            assert!(entries[0].text.contains("Job event"));
            assert!(entries[0].text.contains("details unavailable"));
            assert!(!entries[0].text.contains("<skyhook_"));
        }
    }

    #[test]
    fn failed_request_labels_do_not_confuse_http_retries_with_invocations() {
        for (attempt, error, label) in [
            (
                1,
                "Timeout: provider HTTP startup timeout (after 3 HTTP attempts)",
                "Request failed\n",
            ),
            (2, "Protocol: rejected", "Request failed · attempt 2\n"),
        ] {
            let mut snapshot = ObservationSnapshot::default();
            let root = AgentId::root(SessionId::from_bytes([1; 16]));
            let context = context(&mut snapshot, &root);
            let request = request(&mut snapshot, &root, context);
            record(
                &mut snapshot,
                &root,
                SessionEvent::ModelFailed {
                    request,
                    attempt,
                    error: error.into(),
                },
            );
            let mut projection = Projection::default();
            projection.rebuild(&snapshot);
            let entries = entries(
                &snapshot,
                &projection,
                &root,
                &View::default(),
                &HashMap::new(),
                false,
                false,
            );
            let failure = entries
                .iter()
                .find(|entry| entry.key == format!("failed{request}"))
                .unwrap();
            assert_eq!(failure.text, format!("{label}{error}"));
            assert!(!failure.text.contains("/3"));
        }
    }

    #[test]
    fn recovery_status_is_nonterminal_and_keeps_partial_output() {
        let mut snapshot = ObservationSnapshot::default();
        let root = AgentId::root(SessionId::from_bytes([1; 16]));
        let context = context(&mut snapshot, &root);
        let failed = request(&mut snapshot, &root, context);
        update(
            &mut snapshot,
            delta_event(
                root.clone(),
                failed,
                BlockKind::Text,
                "partial answer".into(),
            ),
        );
        record(
            &mut snapshot,
            &root,
            SessionEvent::ModelFailed {
                request: failed,
                attempt: 1,
                error: "stream lost".into(),
            },
        );
        record(
            &mut snapshot,
            &root,
            SessionEvent::ModelRecoveryScheduled {
                request: failed,
                attempt: 2,
                max_attempts: 3,
                delay_millis: 1000,
                error: "stream lost".into(),
            },
        );
        let mut projection = Projection::default();
        projection.rebuild(&snapshot);
        let info = AgentInfo {
            id: root.clone(),
            name: "skyhook".into(),
            model: "test".into(),
            target: "root".into(),
            owner: None,
            terminal: false,
        };
        assert_eq!(
            projection.status(&info, &snapshot),
            (true, "Reconnecting · attempt 2 of 3".into())
        );
        assert!(projection.jobs.is_empty());
        let rendered = entries(
            &snapshot,
            &projection,
            &root,
            &View::default(),
            &HashMap::new(),
            false,
            false,
        );
        assert_eq!(
            rendered
                .iter()
                .filter(|entry| entry.text.contains("partial answer"))
                .count(),
            1
        );
        assert!(
            !rendered
                .iter()
                .any(|entry| entry.key == format!("failed{failed}"))
        );
        assert!(rendered.iter().any(|entry| entry.surface == Surface::Status
            && entry.text.contains("Reconnecting · attempt 2 of 3")
            && entry.text.contains("1000 ms")
            && entry.text.contains("stream lost")));
        // A retry keeps its predecessor's failed partial response distinct.
        let retry = request(&mut snapshot, &root, context);
        projection.rebuild(&snapshot);
        assert_eq!(
            projection.status(&info, &snapshot),
            (true, "Working".into())
        );
        assert!(snapshot.responses[&(root.clone(), failed)].settled);
        assert!(!snapshot.responses.contains_key(&(root, retry)));
    }

    #[test]
    fn cached_reconnecting_indicator_tracks_activity_changes() {
        let root = AgentId::root(SessionId::from_bytes([1; 16]));
        let mut snapshot = ObservationSnapshot::default();
        let projection = Projection::default();
        let view = View::default();
        let outputs = HashMap::new();
        let mut cache = ContentCache::default();
        let mut rows = Vec::new();
        for activity in [
            AgentActivity::Working,
            AgentActivity::Reconnecting {
                attempt: 2,
                max_attempts: 3,
            },
            AgentActivity::Reconnecting {
                attempt: 3,
                max_attempts: 3,
            },
            AgentActivity::Interrupted,
        ] {
            update(
                &mut snapshot,
                RuntimeEvent::Activity {
                    agent: root.clone(),
                    activity,
                },
            );
            cache.update(
                &mut rows,
                &snapshot,
                &projection,
                EntryView {
                    agent: &root,
                    view: &view,
                    thinking: false,
                    all_details: false,
                },
                &outputs,
                0,
            );
            let expected = entries(&snapshot, &projection, &root, &view, &outputs, false, false);
            assert!(rows == expected);
        }
        assert!(rows.is_empty());
    }

    #[test]
    fn partial_attempts_stay_before_retry_and_followup_while_current_stream_stays_last() {
        let mut snapshot = ObservationSnapshot::default();
        let root = AgentId::root(SessionId::from_bytes([1; 16]));
        let child = root.child(1);
        let context = context(&mut snapshot, &root);
        let failed = request(&mut snapshot, &root, context);
        update(
            &mut snapshot,
            delta_event(
                root.clone(),
                failed,
                BlockKind::Text,
                "failed partial".into(),
            ),
        );
        record(
            &mut snapshot,
            &root,
            SessionEvent::ModelFailed {
                request: failed,
                attempt: 1,
                error: "stream lost".into(),
            },
        );
        let retry = request(&mut snapshot, &root, context);
        record(
            &mut snapshot,
            &root,
            SessionEvent::MessageCommitted {
                message: Message::Assistant(vec![AssistantItem::text(
                    "text",
                    1,
                    "successful retry",
                )]),
            },
        );
        record(
            &mut snapshot,
            &root,
            SessionEvent::MessageCommitted {
                message: Message::User(vec![UserContent::Text {
                    text: "new followup".into(),
                }]),
            },
        );
        let interrupted = request(&mut snapshot, &root, context);
        update(
            &mut snapshot,
            delta_event(
                root.clone(),
                interrupted,
                BlockKind::Text,
                "interrupted partial".into(),
            ),
        );
        update(
            &mut snapshot,
            RuntimeEvent::Activity {
                agent: root.clone(),
                activity: AgentActivity::Interrupted,
            },
        );
        let current = request(&mut snapshot, &root, context);
        update(
            &mut snapshot,
            delta_event(
                root.clone(),
                current,
                BlockKind::Text,
                "current stream".into(),
            ),
        );
        update(
            &mut snapshot,
            delta_event(child, retry, BlockKind::Text, "child only".into()),
        );
        let mut projection = Projection::default();
        projection.rebuild(&snapshot);
        let entries = entries(
            &snapshot,
            &projection,
            &root,
            &View::default(),
            &HashMap::new(),
            false,
            false,
        );
        let text = entries
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for (a, b) in [
            ("failed partial", "successful retry"),
            ("successful retry", "new followup"),
            ("new followup", "interrupted partial"),
            ("interrupted partial", "current stream"),
        ] {
            assert!(
                text.find(a).unwrap() < text.find(b).unwrap(),
                "{a} must precede {b}: {text}"
            );
        }
        assert_eq!(text.matches("failed partial").count(), 1);
        assert!(!text.contains("child only"));
        assert!(entries.last().unwrap().text.contains("current stream"));
    }
}

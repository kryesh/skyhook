use super::{
    format::{agent_label, brief},
    tool_view::{Document, Role},
};
use serde_json::Value;
use skyhook::{
    agent::{AgentActivity, LiveResponse, ObservationSnapshot},
    identity::{AgentId, JobId},
    job::JobState,
    provider::protocol::{BlockContent, BlockKind, Message, Usage, UserContent},
    session::SessionEvent,
};
use std::{
    cell::OnceCell,
    collections::{BTreeMap, HashMap, HashSet},
    time::Instant,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Conversation,
    Requests,
    Jobs,
    State,
}
impl Tab {
    pub fn next(self, backwards: bool) -> Self {
        let tabs = [Self::Conversation, Self::Requests, Self::Jobs, Self::State];
        let n = tabs.iter().position(|t| *t == self).unwrap();
        tabs[(n + if backwards { 3 } else { 1 }) % 4]
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
    pub indent: u16,
    pub job: Option<JobId>,
    pub document: Option<Document>,
    /// Omit the separator before a related sibling tool or this script's first child.
    pub compact_after: bool,
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
            indent: 0,
            job: None,
            document: None,
            compact_after: false,
        }
    }
}
#[derive(Clone)]
pub struct AgentInfo {
    pub id: AgentId,
    pub name: String,
    pub model: String,
    pub profile: Option<String>,
    pub location: String,
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
    failed: Option<String>,
    details: String,
    response: Option<u64>,
    input: OnceCell<String>,
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
    #[cfg(test)]
    request_reconstructions: std::cell::Cell<usize>,
    requests: HashMap<u64, RequestInfo>,
    active_request: HashMap<AgentId, u64>,
    response_requests: HashMap<u64, u64>,
    tool_origins: HashSet<(u64, String)>,
    tool_results: HashSet<(AgentId, String)>,
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
                    agent_profile,
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
                        profile: agent_profile.clone(),
                        location: format!("{} · {}", location.target, location.workspace.display()),
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
                SessionEvent::ModelFailed { request, error, .. } => {
                    let info = self.requests.entry(*request).or_default();
                    info.failed = Some(error.clone());
                    info.finished_millis.get_or_insert(record.timestamp_millis);
                }
                SessionEvent::Usage {
                    request: Some(request),
                    usage,
                } => {
                    let info = self.requests.entry(*request).or_default();
                    add_usage(info.usage.get_or_insert_with(Usage::default), *usage);
                    info.finished_millis.get_or_insert(record.timestamp_millis);
                    info.details
                        .push_str(&format!("\nUsage\n{}", pretty(usage)));
                }
                SessionEvent::Compaction { checkpoint } => {
                    self.requests
                        .entry(checkpoint.request)
                        .or_default()
                        .details
                        .push_str(&format!(
                            "\nAccepted compaction checkpoint\n{}",
                            pretty(checkpoint)
                        ));
                }
                SessionEvent::CompactionSkipped { request, reason } => {
                    self.requests
                        .entry(*request)
                        .or_default()
                        .details
                        .push_str(&format!("\nCompaction skipped\n{reason}"));
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
                    self.tool_origins
                        .insert((origin.message, origin.call_id.clone()));
                    self.tool_results
                        .insert((record.agent.clone(), origin.call_id.clone()));
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
    request_suffixes: HashMap<u64, (usize, usize)>,
    job_indices: HashMap<JobId, usize>,
    invalid_jobs: HashSet<JobId>,
    #[cfg(test)]
    historical_rebuilds: usize,
    #[cfg(test)]
    historical_entries: usize,
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
            self.request_suffixes.clear();
            if view.tab == Tab::Requests {
                for (index, entry) in entries.iter().enumerate() {
                    if view.expanded.contains(&entry.key)
                        && let Some(request) = entry
                            .key
                            .strip_prefix('r')
                            .and_then(|id| id.parse::<u64>().ok())
                    {
                        let suffix_len = snapshot.responses.get(&(agent.clone(), request)).map_or(
                            0,
                            |response| {
                                format!("\nResponse\n{}\n{}", response.reasoning(), response.text())
                                    .len()
                            },
                        );
                        self.request_suffixes
                            .insert(request, (index, entry.text.len() - suffix_len));
                    }
                }
            }
            #[cfg(test)]
            {
                self.historical_rebuilds += 1;
                self.historical_entries += entries.len();
            }
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
            for request in &dirty_responses {
                if let Some(&(index, start)) = self.request_suffixes.get(request)
                    && let Some(response) = snapshot.responses.get(&(agent.clone(), *request))
                {
                    let suffix =
                        format!("\nResponse\n{}\n{}", response.reasoning(), response.text());
                    if entries[index].text[start..] != suffix {
                        entries[index].text.truncate(start);
                        entries[index].text.push_str(&suffix);
                        changes.dirty.push(index);
                    }
                }
            }
            // Refresh only the active request's summary; do not reconstruct its
            // recorded input or rebuild the history on each elapsed-time tick.
            if let Some(&request) = projection.active_request.get(agent)
                && let Some(info) = projection.requests.get(&request)
                && let Some(index) = entries
                    .iter()
                    .position(|entry| entry.key == format!("r{request}"))
            {
                let running = request_running(info, snapshot, projection, agent, request);
                let entry = &mut entries[index];
                let stats = request_stats(info, running);
                if let Some(start) = entry.text.find('\n').map(|offset| offset + 1) {
                    let end = entry.text[start..]
                        .find('\n')
                        .map_or(entry.text.len(), |offset| start + offset);
                    if entry.text[start..end] != stats || entry.running != running {
                        if let Some((_, response_start)) = self.request_suffixes.get_mut(&request) {
                            *response_start = *response_start - (end - start) + stats.len();
                        }
                        entry.text.replace_range(start..end, &stats);
                        entry.running = running;
                        changes.appends.remove(&index);
                        if !changes.dirty.contains(&index) {
                            changes.dirty.push(index);
                        }
                    }
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
            if !has_working {
                entries.insert(end, working_entry(agent, label));
                changes.dirty.extend(end..entries.len());
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
        Tab::State => {
            let mut text =
                projection
                    .agents
                    .iter()
                    .find(|a| &a.id == agent)
                    .map_or(String::new(), |a| {
                        format!(
                            "{}\n{}\nModel: {}\nProfile: {}\nStatus: {}\n",
                            a.name,
                            a.location,
                            a.model,
                            a.profile.as_deref().unwrap_or("default"),
                            projection.status(a, snapshot).1
                        )
                    });
            if let Some(items) = records.iter().rev().find_map(|r| {
                if let SessionEvent::TodosReplaced { items } = &r.event {
                    Some(items)
                } else {
                    None
                }
            }) {
                text.push_str("\nTodos\n");
                for item in items {
                    text.push_str(&format!("  {:?}  {}\n", item.status, item.text));
                }
            }
            text.push_str(&format!(
                "\nUsage for this agent\nOutput · Input (uncached) · Context\n{}",
                agent_footer(snapshot, projection, agent),
            ));
            vec![Entry::new("state".into(), text, Surface::Muted)]
        }
        Tab::Requests => records
            .iter()
            .filter_map(|r| {
                if let SessionEvent::ModelRequested { purpose, .. } = &r.event {
                    let key = format!("r{}", r.sequence);
                    let open = view.expanded.contains(&key);
                    let info = projection.requests.get(&r.sequence)?;
                    let failed = info.failed.as_deref();
                    let mut text = format!(
                        "{} Request #{} · {:?}{}",
                        if open { "▾" } else { "▸" },
                        r.sequence,
                        purpose,
                        failed.map_or(String::new(), |e| format!(" · Failed: {e}"))
                    );
                    let running = request_running(info, snapshot, projection, agent, r.sequence);
                    text.push('\n');
                    text.push_str(&request_stats(info, running));
                    if open {
                        text.push_str(info.input.get_or_init(|| {
                            #[cfg(test)]
                            projection
                                .request_reconstructions
                                .set(projection.request_reconstructions.get() + 1);
                            match skyhook::session::reconstruct_model_request_indexed(
                                &snapshot.records,
                                r.sequence,
                            ) {
                                Ok((provider, request)) => format!(
                                    "\nProvider: {provider}\nRecorded provider-neutral input\n{}",
                                    pretty(&request)
                                ),
                                Err(error) => format!("\n{error}"),
                            }
                        }));
                        text.push_str(&info.details);
                        if let Some(response) = snapshot.responses.get(&(agent.clone(), r.sequence))
                        {
                            text.push_str(&format!(
                                "\nResponse\n{}\n{}",
                                response.reasoning(),
                                response.text()
                            ));
                        } else if let Some(record) = info
                            .response
                            .and_then(|sequence| snapshot.records.get(&sequence))
                            && let SessionEvent::MessageCommitted {
                                message: Message::Assistant(response),
                            } = &record.event
                        {
                            text.push_str(&format!("\nCommitted response\n{}", pretty(response)));
                        }
                    }
                    let mut e = Entry::new(key, text, Surface::Tool);
                    e.expandable = true;
                    e.running = running;
                    Some(e)
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
                                        if text
                                            .trim_start()
                                            .starts_with("<skyhook_job_events>") =>
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
                                    matches!(&block.content, BlockContent::Text { text } if !text.is_empty())
                                })
                            };
                            for (i, (item, block)) in blocks.iter().enumerate() {
                                let block_key = response_block_key(request, &item.id, &block.id);
                                match &block.content {
                                    BlockContent::Text { text } if !text.is_empty() => {
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
                                            .contains(&(record.sequence, call.id.clone()));
                                        if !exists {
                                            let key = block_key.clone();
                                            let open = view.is_expanded(&key, all_details);
                                            let header = format!(
                                                "{} {}{}",
                                                if open { "▾" } else { "▸" },
                                                call.name,
                                                if call.name == "agent" {
                                                    target_suffix(
                                                        projection
                                                            .child_target(agent, &call.arguments),
                                                    )
                                                } else {
                                                    String::new()
                                                },
                                            );
                                            let mut e =
                                                Entry::new(key, header.clone(), Surface::Tool);
                                            if open {
                                                let mut document = Document::default();
                                                document.line(header, Role::Heading);
                                                document.arguments(&call.name, &call.arguments);
                                                e.text = document.plain_text();
                                                e.document = Some(document);
                                            }
                                            e.expandable = true;
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
                            for result in results {
                                if !projection
                                    .tool_results
                                    .contains(&(agent.clone(), result.call_id.clone()))
                                {
                                    entries.push(Entry::new(
                                        format!("{key}/{}", result.call_id),
                                        format!(
                                            "{} result\n{}",
                                            result.name,
                                            pretty(&result.result)
                                        ),
                                        if result.is_error {
                                            Surface::Error
                                        } else {
                                            Surface::Tool
                                        },
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
                        entries.push(Entry::new(
                            format!("failed{request}"),
                            format!("Request failed · attempt {attempt}/3\n{error}"),
                            Surface::Error,
                        ));
                    }
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
                entries.push(working_entry(agent, label));
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
) -> Option<&'static str> {
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
            "Working"
        }
        Some(AgentActivity::Compacting) => "Compacting",
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
                BlockKind::Text if !block.text.is_empty() => {
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

fn request_stats(info: &RequestInfo, running: bool) -> String {
    let usage = info.usage.map_or_else(
        || "Out — · In — · Cached —".into(),
        |usage| {
            format!(
                "Out {} · In {} · Cached {}",
                number(usage.output_tokens),
                number(usage.input_tokens),
                number(usage.cached_input_tokens)
            )
        },
    );
    let timing = match (info.started_millis, info.finished_millis) {
        (Some(start), Some(end)) => {
            format!("{:.1}s", end.saturating_sub(start).max(0) as f64 / 1000.0)
        }
        (Some(start), None) if running => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(start, |duration| {
                    duration.as_millis().min(i64::MAX as u128) as i64
                });
            format!(
                "{:.1}s elapsed",
                now.saturating_sub(start).max(0) as f64 / 1000.0
            )
        }
        _ => "Time —".into(),
    };
    format!("{usage} · {timing}")
}

/// Show the event where the model received it, using its historical payload
/// rather than the job's latest output (the same job may have since resumed).
fn job_event_entries(
    key: &str,
    text: &str,
    projection: &Projection,
    view: &View,
    all: bool,
) -> Vec<Entry> {
    let events = text
        .trim()
        .strip_prefix("<skyhook_job_events>")
        .and_then(|text| text.strip_suffix("</skyhook_job_events>"))
        .and_then(|json| serde_json::from_str::<Vec<Value>>(json).ok());
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
                .unwrap_or("job");
            let state = event
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("updated");
            let heading = format!(
                "{} Job event · {}{} · {}",
                if open { "▾" } else { "▸" },
                tool,
                id.map_or(String::new(), |id| format!(" #{id}")),
                state
            );
            let mut entry = Entry::new(key, heading.clone(), Surface::Tool);
            entry.expandable = true;
            // Deliberately not Entry.job: output refresh must not replace this
            // historical notification with a live job card or discard its key.
            if open {
                let mut body = Document::default();
                body.line(heading, Role::Heading);
                body.line("Notification received by model", Role::Muted);
                body.output(tool, job.map_or(&Value::Null, |job| &job.args), event);
                entry.text = body.plain_text();
                entry.document = Some(body);
            }
            entry
        })
        .collect()
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
    let target = target_suffix(projection.job_target(job));
    let mut text = format!(
        "{} {symbol} {}{} {} · {} · #{}",
        if open { "▾" } else { "▸" },
        job.tool,
        target,
        brief(&detail, 90),
        state_name(job.state),
        job.id
    );
    if let Some(error) = &job.error {
        text.push_str(&format!("\n{error}"));
    }
    let mut document = None;
    if open {
        let mut body = Document::default();
        // Header/error lines retain their separate semantics in the UI document.
        body.line(text.clone(), Role::Heading);
        body.line(job.location.clone(), Role::Muted);
        body.arguments(&job.tool, &job.args);
        if let Some(output) = outputs.get(&job.id) {
            body.output(&job.tool, &job.args, output);
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
            AssistantBlock, AssistantItem, BlockContent, BlockKind, ContentDelta, ItemKind,
            ModelRequest, ReplayEnvelope, ResponseEvent, events_for_content,
        },
        session::{ContextMessage, EventRecord, ModelPurpose},
    };
    #[test]
    fn native_live_display_preserves_interleaved_reasoning_and_text() {
        let agent = AgentId::root(SessionId::from_bytes([91; 16]));
        let mut snapshot = ObservationSnapshot::default();
        for (id, position, kind, block_kind, text) in [
            (
                "second",
                2,
                ItemKind::Reasoning,
                BlockKind::Reasoning,
                "second",
            ),
            (
                "first",
                0,
                ItemKind::Reasoning,
                BlockKind::Reasoning,
                "first",
            ),
            ("answer", 1, ItemKind::Text, BlockKind::Text, "answer"),
        ] {
            response_event(
                &mut snapshot,
                &agent,
                7,
                ResponseEvent::ItemStarted {
                    id: id.into(),
                    position,
                    kind,
                },
            );
            response_event(
                &mut snapshot,
                &agent,
                7,
                ResponseEvent::BlockStarted {
                    item: id.into(),
                    id: format!("{id}:0"),
                    position: 0,
                    kind: block_kind,
                },
            );
            response_event(
                &mut snapshot,
                &agent,
                7,
                ResponseEvent::BlockDelta {
                    item: id.into(),
                    block: format!("{id}:0"),
                    delta: ContentDelta::Text(text.into()),
                },
            );
        }
        response_event(
            &mut snapshot,
            &agent,
            7,
            ResponseEvent::BlockEnded {
                item: "first".into(),
                block: "first:0".into(),
                content: BlockContent::Reasoning {
                    text: "first complete".into(),
                },
            },
        );
        let entries = response_entries(
            7,
            &snapshot.responses[&(agent, 7)],
            &View::default(),
            true,
            "Agent",
        );
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].surface, Surface::Reasoning);
        assert!(entries[0].text.contains("first complete"));
        assert!(!entries[0].running);
        assert_eq!(entries[1].text, "Agent\nanswer");
        assert_eq!(entries[2].surface, Surface::Reasoning);
        assert!(entries[2].text.contains("second"));
        assert!(entries[2].running);
    }

    #[test]
    fn consecutive_reasoning_blocks_end_independently_and_keep_identity_on_replay() {
        let agent = AgentId::root(SessionId::from_bytes([92; 16]));
        let mut snapshot = ObservationSnapshot::default();
        let context = context(&mut snapshot, &agent);
        let request = request(&mut snapshot, &agent, context);
        let mut view = View::default();
        let summaries = [
            "First\nSecond\nThird",
            "Fourth\nFifth\nSixth",
            "Seventh\nEighth",
        ];
        let mut blocks = Vec::new();
        let mut keys = Vec::new();
        response_event(
            &mut snapshot,
            &agent,
            request,
            ResponseEvent::ItemStarted {
                id: "native-reasoning".into(),
                position: 0,
                kind: ItemKind::Reasoning,
            },
        );
        for (position, text) in summaries.into_iter().enumerate() {
            let id = format!("summary-{position}");
            response_event(
                &mut snapshot,
                &agent,
                request,
                ResponseEvent::BlockStarted {
                    item: "native-reasoning".into(),
                    id: id.clone(),
                    position,
                    kind: BlockKind::Reasoning,
                },
            );
            response_event(
                &mut snapshot,
                &agent,
                request,
                ResponseEvent::BlockDelta {
                    item: "native-reasoning".into(),
                    block: id.clone(),
                    delta: ContentDelta::Text(format!("provisional\n{text}")),
                },
            );
            let live = &snapshot.responses[&(agent.clone(), request)];
            let rows = response_entries(request, live, &view, false, "Agent");
            assert_eq!(rows.len(), position + 1);
            assert_eq!(rows.iter().filter(|entry| entry.running).count(), 1);
            assert!(rows[position].running && rows[position].default_open);
            assert!(rows[position].text.contains("provisional"));
            keys.push(rows[position].key.clone());
            response_event(
                &mut snapshot,
                &agent,
                request,
                ResponseEvent::BlockEnded {
                    item: "native-reasoning".into(),
                    block: id.clone(),
                    content: BlockContent::Reasoning { text: text.into() },
                },
            );
            let live = &snapshot.responses[&(agent.clone(), request)];
            assert!(!live.settled);
            assert!(!live.snapshot().items[0].ended, "item replay arrives later");
            assert!(
                live.text().is_empty(),
                "block closure does not need an answer"
            );
            let closed = response_entries(request, live, &view, false, "Agent");
            assert!(closed.iter().all(|entry| !entry.running));
            assert_eq!(closed[position].text, "▸ Reasoning");
            view.expanded.insert(keys[position].clone());
            let expanded = response_entries(request, live, &view, false, "Agent");
            assert!(expanded[position].text.ends_with(text));
            assert!(
                !expanded[position].text.contains("provisional"),
                "authoritative end replaces deltas"
            );
            blocks.push(AssistantBlock {
                id,
                position,
                content: BlockContent::Reasoning { text: text.into() },
            });
        }
        response_event(
            &mut snapshot,
            &agent,
            request,
            ResponseEvent::ItemEnded {
                id: "native-reasoning".into(),
                replay: Some(replay()),
            },
        );
        let live = &snapshot.responses[&(agent.clone(), request)];
        assert_eq!(live.snapshot().items[0].replay, Some(replay()));
        assert_eq!(
            response_entries(request, live, &view, false, "Agent")
                .iter()
                .map(|entry| entry.key.clone())
                .collect::<Vec<_>>(),
            keys
        );
        let answer = AssistantItem::text("answer", 1, "Answer");
        for event in events_for_content(std::slice::from_ref(&answer)) {
            response_event(&mut snapshot, &agent, request, event);
        }
        record(
            &mut snapshot,
            &agent,
            SessionEvent::MessageCommitted {
                message: Message::Assistant(vec![
                    AssistantItem {
                        id: "native-reasoning".into(),
                        position: 0,
                        kind: ItemKind::Reasoning,
                        blocks,
                        replay: Some(replay()),
                    },
                    answer,
                ]),
            },
        );
        let records: Vec<EventRecord> = serde_json::from_slice(
            &serde_json::to_vec(&snapshot.records.values().collect::<Vec<_>>()).unwrap(),
        )
        .unwrap();
        let mut persisted = ObservationSnapshot::default();
        for record in records {
            update(&mut persisted, RuntimeEvent::Record(Box::new(record)));
        }
        for snapshot in [&snapshot, &persisted] {
            let mut projection = Projection::default();
            projection.rebuild(snapshot);
            let rows = entries(
                snapshot,
                &projection,
                &agent,
                &view,
                &HashMap::new(),
                false,
                false,
            );
            let reasoning = rows
                .iter()
                .filter(|entry| entry.surface == Surface::Reasoning)
                .collect::<Vec<_>>();
            assert_eq!(reasoning.len(), 3, "live and committed must not duplicate");
            assert_eq!(
                reasoning
                    .iter()
                    .map(|entry| entry.key.clone())
                    .collect::<Vec<_>>(),
                keys
            );
            for (entry, text) in reasoning.into_iter().zip(summaries) {
                assert!(!entry.running);
                assert!(
                    entry.text.ends_with(text),
                    "expanded identity survives handoff"
                );
            }
        }
    }

    #[test]
    fn job_notifications_are_historical_tool_cards_between_assistant_responses() {
        let agent = AgentId::root(SessionId::from_bytes([48; 16]));
        let payload = "<skyhook_job_events>\n[{\"id\":7,\"tool\":\"exec\",\"state\":\"completed\",\"result\":{\"stdout\":\"historical output\"}},{\"id\":8,\"state\":\"failed\",\"error\":\"historical failure\"}]\n</skyhook_job_events>";
        let mut snapshot = ObservationSnapshot::default();
        for message in [
            Message::Assistant(vec![AssistantItem::text("text-1", 1, "First answer")]),
            Message::User(vec![UserContent::Runtime {
                text: payload.into(),
            }]),
            Message::Assistant(vec![AssistantItem::text("text-2", 2, "Follow-up answer")]),
        ] {
            record(
                &mut snapshot,
                &agent,
                SessionEvent::MessageCommitted { message },
            );
        }
        let mut projection = Projection::default();
        projection.rebuild(&snapshot);
        for open in [false, true] {
            let rows = entries(
                &snapshot,
                &projection,
                &agent,
                &View::default(),
                &HashMap::new(),
                true,
                open,
            );
            assert_eq!(rows.len(), 4);
            assert!(rows[0].text.contains("First answer"));
            assert!(rows[3].text.contains("Follow-up answer"));
            assert!(rows[1].text.contains("Job event · exec #7 · completed"));
            assert!(rows[2].text.contains("Job event · job #8 · failed"));
            assert!(rows[1].compact_after);
            assert!(!rows[2].compact_after);
            for row in &rows[1..3] {
                assert!(row.surface == Surface::Tool);
                assert!(row.expandable);
                assert!(row.job.is_none());
                assert!(!row.text.contains("skyhook_job_events"));
                assert!(!row.text.contains("Harness notification"));
            }
            assert_eq!(rows[1].text.contains("historical output"), open);
            assert_eq!(rows[2].text.contains("historical failure"), open);
        }
        // Presentation does not rewrite the actual model input.
        assert!(snapshot.records.values().any(|r| matches!(&r.event,
            SessionEvent::MessageCommitted { message: Message::User(blocks) }
                if blocks.iter().any(|b| matches!(b, UserContent::Runtime { text } if text == payload))
        )));
        for text in [
            "<skyhook_job_events>bad json</skyhook_job_events>",
            "<skyhook_job_events>[]</skyhook_job_events>",
        ] {
            let rows = job_event_entries("test", text, &projection, &View::default(), true);
            assert_eq!(rows.len(), 1);
            assert!(rows[0].text.contains("details unavailable"));
            assert!(!rows[0].text.contains("skyhook_job_events"));
        }
    }

    #[test]
    fn conversation_compacts_only_adjacent_calls_from_the_same_response() {
        let agent = AgentId::root(SessionId::from_bytes([44; 16]));
        let mut snapshot = ObservationSnapshot::default();
        let call = |id: &str| {
            AssistantItem::tool_call(
                id,
                0,
                skyhook::provider::protocol::ToolCall {
                    id: id.into(),
                    name: "exec".into(),
                    arguments: serde_json::json!({"argv": ["true"]}),
                },
            )
        };
        for blocks in [
            vec![
                call("a"),
                call("b"),
                AssistantItem::text("text-3", 3, "Next"),
                call("c"),
                AssistantItem::reasoning("reasoning-11", 11, "Keep this reasoning padded", None),
                call("f"),
            ],
            vec![call("d"), call("e")],
        ] {
            record(
                &mut snapshot,
                &agent,
                SessionEvent::MessageCommitted {
                    message: Message::Assistant(
                        blocks
                            .into_iter()
                            .enumerate()
                            .map(|(position, mut item)| {
                                item.position = position;
                                item
                            })
                            .collect(),
                    ),
                },
            );
        }
        let mut projection = Projection::default();
        projection.rebuild(&snapshot);
        for open in [false, true] {
            let rows = entries(
                &snapshot,
                &projection,
                &agent,
                &View::default(),
                &HashMap::new(),
                true,
                open,
            );
            assert_eq!(
                rows.iter().map(|e| e.compact_after).collect::<Vec<_>>(),
                [true, false, false, false, false, false, true, false]
            );
        }
    }

    #[test]
    fn conversation_compacts_script_siblings_without_crossing_group_boundaries() {
        let agent = AgentId::root(SessionId::from_bytes([46; 16]));
        let mut snapshot = ObservationSnapshot::default();
        // Script IDs deliberately overlap response IDs: the two identity
        // namespaces must never group together. Both scripts share a response.
        for (id, tool, parent, response) in [
            (100, "script", None, Some(100)),
            (200, "script", None, Some(100)),
            (1, "exec", Some(100), None),
            (2, "read", Some(100), None),
            (3, "exec", Some(200), None),
            (4, "read", Some(200), None),
            (5, "exec", None, Some(200)),
            (6, "read", None, Some(300)),
            (300, "agent", None, None),
            (7, "exec", Some(300), None),
            (8, "read", Some(300), None),
            (400, "script", Some(100), None),
            (9, "exec", Some(400), None),
            (10, "read", Some(400), None),
            (11, "exec", Some(100), None),
        ] {
            record(
                &mut snapshot,
                &agent,
                SessionEvent::JobCreated {
                    job: JobId::new(id).unwrap(),
                    parent: parent.map(|id| JobId::new(id).unwrap()),
                    origin: response.map(|message| skyhook::session::ModelCallOrigin {
                        message,
                        call_id: format!("call{id}"),
                    }),
                    tool: tool.into(),
                    name: None,
                    arguments: serde_json::json!({}),
                    output_schema: None,
                    accepts_input: false,
                    background: false,
                    location: skyhook::execution::ExecutionLocation::root("/tmp".into()),
                },
            );
        }
        let mut projection = Projection::default();
        projection.rebuild(&snapshot);
        for open in [false, true] {
            let rows = entries(
                &snapshot,
                &projection,
                &agent,
                &View::default(),
                &HashMap::new(),
                true,
                open,
            );
            assert_eq!(rows.len(), 15);
            assert_eq!(
                rows.iter()
                    .filter(|entry| entry.compact_after)
                    .map(|entry| entry.key.as_str())
                    .collect::<Vec<_>>(),
                ["j100", "j1", "j3", "j400", "j9"],
            );
        }
    }

    #[test]
    fn conversation_script_sibling_spacing_updates_cached_rows() {
        let agent = AgentId::root(SessionId::from_bytes([47; 16]));
        let mut snapshot = ObservationSnapshot::default();
        let mut projection = Projection::default();
        let mut cache = ContentCache::default();
        let mut rows = Vec::new();
        let view = View::default();
        let presentation = EntryView {
            agent: &agent,
            view: &view,
            thinking: true,
            all_details: true,
        };
        let outputs = HashMap::new();
        for id in 1..=4 {
            if id == 4 {
                record(
                    &mut snapshot,
                    &agent,
                    SessionEvent::Status {
                        message: "Separate script siblings around non-tool content".into(),
                    },
                );
            }
            record(
                &mut snapshot,
                &agent,
                SessionEvent::JobCreated {
                    job: JobId::new(id).unwrap(),
                    parent: (id != 1).then(|| JobId::new(1).unwrap()),
                    origin: None,
                    tool: if id == 1 { "script" } else { "exec" }.into(),
                    name: None,
                    arguments: serde_json::json!({}),
                    output_schema: None,
                    accepts_input: false,
                    background: false,
                    location: skyhook::execution::ExecutionLocation::root("/tmp".into()),
                },
            );
            projection.rebuild(&snapshot);
            let changes =
                cache.update(&mut rows, &snapshot, &projection, presentation, &outputs, 0);
            if id == 2 {
                // The first child removes the script parent's trailing gap and
                // invalidates its already-rendered row, even without an origin.
                assert!(!changes.reset);
                assert_eq!(changes.dirty, [0, 1]);
                assert!(rows[0].compact_after);
            }
            if id == 3 {
                assert!(!changes.reset);
                assert_eq!(changes.dirty, [1, 2]);
                assert!(rows[1].compact_after);
            }
        }
        assert_eq!(
            rows.iter()
                .map(|entry| entry.compact_after)
                .collect::<Vec<_>>(),
            [true, true, false, false, false],
        );
        cache.invalidate_job(JobId::new(2).unwrap());
        let changes = cache.update(&mut rows, &snapshot, &projection, presentation, &outputs, 0);
        assert_eq!(changes.dirty, [1]);
        assert!(rows[1].compact_after);
    }

    #[test]
    fn conversation_job_spacing_invalidates_previous_rows_and_survives_output_refresh() {
        let agent = AgentId::root(SessionId::from_bytes([45; 16]));
        let mut snapshot = ObservationSnapshot::default();
        let mut projection = Projection::default();
        let mut cache = ContentCache::default();
        let mut rows = Vec::new();
        let view = View::default();
        let presentation = EntryView {
            agent: &agent,
            view: &view,
            thinking: true,
            all_details: true,
        };
        let outputs = HashMap::new();
        // Origins, not adjacency or the tool name, define response membership.
        for (id, response) in [
            (1, Some(100)),
            (2, Some(100)),
            (3, Some(200)),
            (4, None),
            (5, None),
        ] {
            record(
                &mut snapshot,
                &agent,
                SessionEvent::JobCreated {
                    job: JobId::new(id).unwrap(),
                    parent: None,
                    origin: response.map(|message| skyhook::session::ModelCallOrigin {
                        message,
                        call_id: format!("call{id}"),
                    }),
                    tool: "exec".into(),
                    name: None,
                    arguments: serde_json::json!({"argv": ["true"]}),
                    output_schema: None,
                    accepts_input: false,
                    background: false,
                    location: skyhook::execution::ExecutionLocation::root("/tmp".into()),
                },
            );
            projection.rebuild(&snapshot);
            let changes =
                cache.update(&mut rows, &snapshot, &projection, presentation, &outputs, 0);
            if id == 2 {
                assert!(!changes.reset);
                assert_eq!(changes.dirty, [0, 1]);
                assert!(rows[0].compact_after);
            }
        }
        assert_eq!(
            rows.iter().map(|e| e.compact_after).collect::<Vec<_>>(),
            [true, false, false, false, false]
        );
        cache.invalidate_job(JobId::new(1).unwrap());
        let changes = cache.update(&mut rows, &snapshot, &projection, presentation, &outputs, 0);
        assert_eq!(changes.dirty, [0]);
        assert!(rows[0].compact_after);

        // Removing a neighbor must restore the surviving entry's separator too.
        let old = rows.clone();
        rows.truncate(1);
        rows[0].compact_after = false;
        let mut changes = ContentChanges::default();
        cache.finish_reset(&mut rows, old, &mut changes);
        assert!(!changes.reset);
        assert_eq!(changes.dirty, [0]);
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
        let stable_text = rows[1].text.as_ptr();
        let stable_sections = rows[1].document.as_ref().unwrap().sections.as_ptr();
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
        assert_eq!(rows[1].text.as_ptr(), stable_text);
        assert_eq!(
            rows[1].document.as_ref().unwrap().sections.as_ptr(),
            stable_sections
        );
        assert_eq!(cache.historical_rebuilds, 1);
        assert!(rows[0].text.contains("new output"));
    }

    #[test]
    fn content_cache_streams_without_replacing_unchanged_history_allocations() {
        let agent = AgentId::root(SessionId::from_bytes([31; 16]));
        let mut snapshot = ObservationSnapshot::default();
        for _ in 0..100 {
            record(
                &mut snapshot,
                &agent,
                SessionEvent::MessageCommitted {
                    message: Message::User(vec![UserContent::Text {
                        text: "history".repeat(4096),
                    }]),
                },
            );
        }
        let context = context(&mut snapshot, &agent);
        let request = request(&mut snapshot, &agent, context);
        let mut projection = Projection::default();
        projection.rebuild(&snapshot);
        let view = View::default();
        let outputs = HashMap::new();
        let mut cache = ContentCache::default();
        let mut rows = Vec::new();
        assert!(
            cache
                .update(
                    &mut rows,
                    &snapshot,
                    &projection,
                    EntryView {
                        agent: &agent,
                        view: &view,
                        thinking: false,
                        all_details: false
                    },
                    &outputs,
                    0
                )
                .reset
        );
        let history_pointer = rows[0].text.as_ptr();
        for n in 0..50 {
            update(
                &mut snapshot,
                delta_event(agent.clone(), request, BlockKind::Text, "chunk".into()),
            );
            cache.observe_response(&agent, request);
            let changes = cache.update(
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
            assert!(!changes.reset);
            assert_eq!(rows[0].text.as_ptr(), history_pointer);
            assert_eq!(cache.historical_rebuilds, 1);
            assert_eq!(cache.historical_entries, 100);
            assert!(changes.dirty.iter().all(|i| *i >= 100));
            assert_eq!(rows[100].text, format!("Agent\n{}", "chunk".repeat(n + 1)));
        }
        // Presentation/output invalidation is explicit and conservative.
        cache.observe_response(&agent, request);
        let changes = cache.update(
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
            1,
        );
        assert!(!changes.reset);
        assert!(changes.dirty.is_empty());
        assert_eq!(cache.historical_rebuilds, 2);
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
        let provisional = snapshot.responses[&(agent.clone(), request)].reasoning();
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
        assert_eq!(cache.historical_rebuilds, 1);
    }

    #[test]
    fn content_cache_expanded_request_updates_without_reconstructing_input() {
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
            assert_eq!(rows[0].text, expected[0].text);
        }
        assert_eq!(projection.request_reconstructions.get(), 1);
        assert_eq!(cache.historical_rebuilds, 1);
    }

    #[test]
    fn arguments_show_named_fields_nested_lists_and_literal_multiline_text() {
        let mut document = Document::default();
        document.arguments(
            "exec",
            &serde_json::json!({
                "argv": ["printf", "hello world"],
                "options": {"enabled": true, "limit": null},
                "script": "const value = {a: 1};\nreturn value;"
            }),
        );
        let rendered = document.plain_text();
        assert_eq!(
            rendered,
            "Arguments\n  argv\n    1.  printf\n    2.  hello world\n  options\n    enabled  true\n    limit    none\n  script\n    const value = {a: 1};\n    return value;"
        );
        let mut empty = Document::default();
        empty.arguments("exec", &serde_json::json!({}));
        assert_eq!(empty.plain_text(), "Arguments\n  No arguments");
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
    fn interrupted_reasoning_collapses_in_place_and_stays_with_its_agent() {
        let agent = AgentId::root(SessionId::from_bytes([1; 16]));
        let mut snapshot = ObservationSnapshot::default();
        let context = context(&mut snapshot, &agent);
        let request = request(&mut snapshot, &agent, context);
        for owner in [agent.clone(), agent.child(1)] {
            update(
                &mut snapshot,
                delta_event(
                    owner.clone(),
                    request,
                    BlockKind::Reasoning,
                    if owner == agent {
                        "Partial reasoning"
                    } else {
                        "Child reasoning"
                    }
                    .into(),
                ),
            );
        }
        update(
            &mut snapshot,
            RuntimeEvent::Activity {
                agent: agent.clone(),
                activity: AgentActivity::Interrupted,
            },
        );
        record(
            &mut snapshot,
            &agent,
            SessionEvent::Status {
                message: "Interrupted".into(),
            },
        );
        let mut projection = Projection::default();
        projection.rebuild(&snapshot);
        let mut view = View::default();
        let rows = entries(
            &snapshot,
            &projection,
            &agent,
            &view,
            &HashMap::new(),
            false,
            false,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "Partial reasoning");
        assert!(!rows[0].expandable);
        assert_eq!(rows[1].text, "Status · Interrupted");
        view.expanded.insert(rows[0].key.clone());
        let rows = entries(
            &snapshot,
            &projection,
            &agent,
            &view,
            &HashMap::new(),
            false,
            false,
        );
        assert!(rows[0].text.contains("Partial reasoning"));
        assert!(!rows[0].text.contains("Child reasoning"));
        let child = entries(
            &snapshot,
            &projection,
            &agent.child(1),
            &View::default(),
            &HashMap::new(),
            false,
            false,
        );
        assert_eq!(child.len(), 1);
        assert_eq!(child[0].text, "Child reasoning");
        assert!(!child[0].expandable);
    }

    #[test]
    fn reasoning_line_count_controls_folding_and_streams_can_grow_into_blocks() {
        let mut view = View::default();
        view.collapsed.insert("reasoning".into());
        for thinking in [false, true] {
            let single = reasoning_entry(
                "reasoning".into(),
                "\n**A single line**\n",
                &view,
                thinking,
                "Reasoning",
            );
            assert!(!single.expandable);
            assert_eq!(single.text, "**A single line**");
        }
        let multi = reasoning_entry(
            "reasoning".into(),
            "First line\nSecond line",
            &View::default(),
            true,
            "Reasoning",
        );
        assert!(multi.expandable);
        assert!(multi.text.starts_with("▾ Reasoning\n"));
        let completed = reasoning_entry(
            "reasoning".into(),
            "First line\nSecond line",
            &View::default(),
            false,
            "Reasoning",
        );
        assert_eq!(completed.text, "▸ Reasoning");
    }

    #[test]
    fn working_feedback_covers_silent_and_text_streams_without_sticking_after_completion() {
        let agent = AgentId::root(SessionId::from_bytes([3; 16]));
        let mut snapshot = ObservationSnapshot::default();
        let context = context(&mut snapshot, &agent);
        let request = request(&mut snapshot, &agent, context);
        let rows = |snapshot: &ObservationSnapshot| {
            let mut projection = Projection::default();
            projection.rebuild(snapshot);
            entries(
                snapshot,
                &projection,
                &agent,
                &View::default(),
                &HashMap::new(),
                false,
                false,
            )
        };
        assert!(
            rows(&snapshot).is_empty(),
            "replay must not invent running requests"
        );
        update(
            &mut snapshot,
            RuntimeEvent::Activity {
                agent: agent.clone(),
                activity: AgentActivity::Working,
            },
        );
        let waiting = rows(&snapshot);
        assert_eq!(waiting.len(), 1);
        assert!(waiting[0].running && !waiting[0].expandable);
        assert_eq!(waiting[0].text, "  Working");
        update(
            &mut snapshot,
            delta_event(
                agent.child(1),
                request,
                BlockKind::Reasoning,
                "Child only".into(),
            ),
        );
        assert_eq!(rows(&snapshot)[0].text, "  Working");
        update(
            &mut snapshot,
            delta_event(
                agent.clone(),
                request,
                BlockKind::Reasoning,
                "One step".into(),
            ),
        );
        let reasoning = rows(&snapshot);
        assert_eq!(reasoning.len(), 1, "reasoning replaces the generic spinner");
        assert!(reasoning[0].running && !reasoning[0].expandable);
        update(
            &mut snapshot,
            delta_event(agent.clone(), request, BlockKind::Text, "Answer".into()),
        );
        assert_eq!(rows(&snapshot).iter().filter(|e| e.running).count(), 1);
        update(
            &mut snapshot,
            RuntimeEvent::Activity {
                agent: agent.clone(),
                activity: AgentActivity::Interrupted,
            },
        );
        assert!(rows(&snapshot).iter().all(|e| !e.running));
        update(
            &mut snapshot,
            RuntimeEvent::Activity {
                agent: agent.clone(),
                activity: AgentActivity::Working,
            },
        );
        record(
            &mut snapshot,
            &agent,
            SessionEvent::MessageCommitted {
                message: Message::Assistant(vec![
                    AssistantItem::reasoning("reasoning", 0, "One step", None),
                    AssistantItem::text("text", 1, "Answer"),
                ]),
            },
        );
        let done = rows(&snapshot);
        assert!(done.iter().all(|e| !e.running));
        assert_eq!(done[0].text, "One step");
        assert!(!done[0].expandable);
    }

    #[test]
    fn final_reply_footers_use_recorded_models_and_skip_intermediate_messages() {
        let agent = AgentId::root(SessionId::from_bytes([9; 16]));
        let mut snapshot = ObservationSnapshot::default();
        for name in ["original-model-id", "replacement-model-id"] {
            let context = context(&mut snapshot, &agent);
            if let SessionEvent::ModelContext { template, .. } =
                &mut snapshot.records.get_mut(&context).unwrap().event
            {
                template.model = name.into();
            }
            request(&mut snapshot, &agent, context);
            record(
                &mut snapshot,
                &agent,
                SessionEvent::MessageCommitted {
                    message: Message::Assistant(vec![
                        AssistantItem::text("text-6", 6, "Checking a file"),
                        AssistantItem::tool_call(
                            "tool",
                            10,
                            skyhook::provider::protocol::ToolCall {
                                id: format!("call-{name}"),
                                name: "read".into(),
                                arguments: serde_json::json!({"path":"file"}),
                            },
                        ),
                    ]),
                },
            );
            request(&mut snapshot, &agent, context);
            record(
                &mut snapshot,
                &agent,
                SessionEvent::MessageCommitted {
                    message: Message::Assistant(vec![
                        AssistantItem::text("text-7", 7, "Answer part one"),
                        AssistantItem::text("text-8", 8, "Answer part two"),
                    ]),
                },
            );
        }
        let mut projection = Projection::default();
        projection.rebuild(&snapshot);
        let rows = entries(
            &snapshot,
            &projection,
            &agent,
            &View::default(),
            &HashMap::new(),
            false,
            false,
        );
        let footers = rows
            .iter()
            .filter_map(|row| row.footer.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(footers, ["original-model-id", "replacement-model-id"]);
        for row in rows.iter().filter(|row| row.footer.is_some()) {
            assert!(row.text.ends_with("Answer part two"));
        }
    }

    #[test]
    fn resumed_child_reopens_on_owner_running_without_another_agent_started() {
        let root = AgentId::root(SessionId::from_bytes([2; 16]));
        let child = root.child(1);
        let job = JobId::new(1).unwrap();
        for interrupted in [false, true] {
            let mut snapshot = ObservationSnapshot::default();
            let mut projection = Projection::default();
            record(
                &mut snapshot,
                &root,
                SessionEvent::JobCreated {
                    job,
                    parent: None,
                    origin: None,
                    tool: "agent".into(),
                    name: Some("worker".into()),
                    arguments: serde_json::json!({"prompt": "inspect"}),
                    output_schema: None,
                    accepts_input: true,
                    background: true,
                    location: skyhook::execution::ExecutionLocation::root("/host".into()),
                },
            );
            record(
                &mut snapshot,
                &child,
                SessionEvent::AgentStarted {
                    parent: Some(root.clone()),
                    owner_job: Some(job),
                    model_profile: "fixture".into(),
                    max_context: None,
                    agent_profile: None,
                    location: skyhook::execution::ExecutionLocation::root("/child".into()),
                },
            );
            record(
                &mut snapshot,
                &root,
                SessionEvent::JobStateChanged {
                    job,
                    state: JobState::Running,
                },
            );
            projection.rebuild(&snapshot);
            assert!(projection.has_active_children());

            // Exercise more than one resume of the same child identity.
            for _ in 0..2 {
                record(
                    &mut snapshot,
                    &child,
                    if interrupted {
                        SessionEvent::AgentInterrupted
                    } else {
                        SessionEvent::AgentCompleted
                    },
                );
                record(
                    &mut snapshot,
                    &root,
                    SessionEvent::JobStateChanged {
                        job,
                        state: JobState::Completed,
                    },
                );
                projection.rebuild(&snapshot);
                assert!(!projection.has_active_children());
                projection.completed.clear(); // completion grace period has expired
                assert!(projection.visible(&root).is_empty());

                // Activity and unrelated tools must not revive a completed child.
                for activity in [AgentActivity::Working, AgentActivity::Tools] {
                    update(
                        &mut snapshot,
                        RuntimeEvent::Activity {
                            agent: child.clone(),
                            activity,
                        },
                    );
                    record(
                        &mut snapshot,
                        &child,
                        SessionEvent::JobStateChanged {
                            job: JobId::new(2).unwrap(),
                            state: JobState::Running,
                        },
                    );
                    projection.rebuild(&snapshot);
                    assert!(!projection.has_active_children());
                }

                // The owner event is recorded on the parent, not the child.
                record(
                    &mut snapshot,
                    &root,
                    SessionEvent::JobStateChanged {
                        job,
                        state: JobState::Running,
                    },
                );
                update(
                    &mut snapshot,
                    RuntimeEvent::Activity {
                        agent: child.clone(),
                        activity: AgentActivity::Working,
                    },
                );
                projection.rebuild(&snapshot);
                assert!(projection.has_active_children());
                assert_eq!(projection.visible(&root).len(), 1);
                assert!(!projection.completed.contains_key(&child));
                assert_eq!(
                    projection.status(&projection.agents[0], &snapshot),
                    (true, "Working".into())
                );
                // Rebuilds and replay must not retain an old terminal marker.
                projection.rebuild(&snapshot);
                let mut replay = Projection::default();
                replay.rebuild(&snapshot);
                assert!(replay.has_active_children());
                assert!(!replay.completed.contains_key(&child));
                assert_eq!(replay.agents.len(), 1);
                assert_eq!(replay.agents[0].owner, Some(job));
            }

            // A completion later in the same snapshot still wins over Running.
            record(&mut snapshot, &child, SessionEvent::AgentCompleted);
            projection.rebuild(&snapshot);
            assert!(!projection.has_active_children());
            projection.rebuild(&snapshot);
            assert!(!projection.has_active_children());
            let mut replay = Projection::default();
            replay.rebuild(&snapshot);
            assert!(!replay.has_active_children());
        }
    }

    #[test]
    fn agent_calls_show_child_targets_before_start_while_running_and_after_replay() {
        for (caller_target, requested, child_target) in [
            ("root", Some("lab-monitoring"), "lab-monitoring"),
            ("lab-monitoring", None, "lab-monitoring"),
            ("lab-monitoring", Some("root"), "root"),
        ] {
            let caller = AgentId::root(SessionId::from_bytes([2; 16]));
            let child = caller.child(1);
            let job = JobId::new(1).unwrap();
            let mut snapshot = ObservationSnapshot::default();
            record(
                &mut snapshot,
                &caller,
                SessionEvent::AgentStarted {
                    parent: None,
                    owner_job: None,
                    model_profile: "fixture".into(),
                    max_context: Some(128000),
                    agent_profile: None,
                    location: skyhook::execution::ExecutionLocation::named(
                        caller_target,
                        "/caller".into(),
                    ),
                },
            );
            let arguments = serde_json::json!({"prompt":"inspect", "target":requested});
            let message = record(
                &mut snapshot,
                &caller,
                SessionEvent::MessageCommitted {
                    message: Message::Assistant(vec![AssistantItem::tool_call(
                        "tool",
                        10,
                        skyhook::provider::protocol::ToolCall {
                            id: "delegate".into(),
                            name: "agent".into(),
                            arguments: arguments.clone(),
                        },
                    )]),
                },
            );
            let mut projection = Projection::default();
            projection.rebuild(&snapshot);
            let expected = format!("agent{}", target_suffix(child_target));
            let pending = entries(
                &snapshot,
                &projection,
                &caller,
                &View::default(),
                &HashMap::new(),
                false,
                false,
            );
            assert!(pending[0].text.contains(&expected));
            record(
                &mut snapshot,
                &caller,
                SessionEvent::JobCreated {
                    job,
                    parent: None,
                    origin: Some(skyhook::session::ModelCallOrigin {
                        message,
                        call_id: "delegate".into(),
                    }),
                    tool: "agent".into(),
                    name: None,
                    arguments,
                    output_schema: None,
                    accepts_input: true,
                    background: false,
                    // Agent orchestration executes on the host, regardless of the child target.
                    location: skyhook::execution::ExecutionLocation::root("/host".into()),
                },
            );
            for state in [JobState::Queued, JobState::Running, JobState::Completed] {
                record(
                    &mut snapshot,
                    &caller,
                    SessionEvent::JobStateChanged { job, state },
                );
                if state == JobState::Running {
                    record(
                        &mut snapshot,
                        &child,
                        SessionEvent::AgentStarted {
                            parent: Some(caller.clone()),
                            owner_job: Some(job),
                            model_profile: "fixture".into(),
                            max_context: Some(128000),
                            agent_profile: None,
                            location: skyhook::execution::ExecutionLocation::named(
                                child_target,
                                "/child".into(),
                            ),
                        },
                    );
                }
                snapshot.records =
                    serde_json::from_slice(&serde_json::to_vec(&snapshot.records).unwrap())
                        .unwrap();
                projection = Projection::default();
                projection.rebuild(&snapshot);
                for tab in [Tab::Conversation, Tab::Jobs] {
                    for expanded in [false, true] {
                        let view = View {
                            tab,
                            ..Default::default()
                        };
                        let rows = entries(
                            &snapshot,
                            &projection,
                            &caller,
                            &view,
                            &HashMap::new(),
                            false,
                            expanded,
                        );
                        assert_eq!(rows.len(), 1);
                        let header = rows[0].text.lines().next().unwrap();
                        assert!(header.contains(&expected), "{header}");
                        assert!(!header.contains("@root"));
                        assert!(header.contains(state_name(state)));
                    }
                }
                if state != JobState::Queued {
                    assert_eq!(
                        projection
                            .agents
                            .iter()
                            .find(|agent| agent.id == child)
                            .unwrap()
                            .target,
                        child_target
                    );
                }
            }
        }
    }

    #[test]
    fn tool_previews_use_recorded_targets_across_views_states_and_replay() {
        let agent = AgentId::root(SessionId::from_bytes([1; 16]));
        let mut snapshot = ObservationSnapshot::default();
        for (id, parent, target) in [
            (1, None, "root"),
            (2, Some(1), "lab-monitoring"),
            (3, Some(2), "root"),
        ] {
            record(
                &mut snapshot,
                &agent,
                SessionEvent::JobCreated {
                    job: JobId::new(id).unwrap(),
                    parent: parent.map(|id| JobId::new(id).unwrap()),
                    origin: None,
                    tool: "exec".into(),
                    name: None,
                    // Inherited targets are not necessarily present in the arguments.
                    arguments: serde_json::json!({"argv": ["ls", "/a/very/long/path/that/must/not/push/the/target/out/of/the/preview"]}),
                    output_schema: None,
                    accepts_input: false,
                    background: false,
                    location: skyhook::execution::ExecutionLocation::named(
                        target,
                        "/workspace".into(),
                    ),
                },
            );
        }
        // Fresh projection from serialized records follows the same path as session resume.
        snapshot.records =
            serde_json::from_slice(&serde_json::to_vec(&snapshot.records).unwrap()).unwrap();
        let mut projection = Projection::default();
        projection.rebuild(&snapshot);
        for state in [
            JobState::Queued,
            JobState::Running,
            JobState::AwaitingApproval,
            JobState::WaitingInput,
            JobState::Completed,
            JobState::Failed,
            JobState::Cancelled,
        ] {
            for id in 1..=3 {
                record(
                    &mut snapshot,
                    &agent,
                    SessionEvent::JobStateChanged {
                        job: JobId::new(id).unwrap(),
                        state,
                    },
                );
            }
            projection.rebuild(&snapshot);
            for tab in [Tab::Conversation, Tab::Jobs] {
                for expanded in [false, true] {
                    let view = View {
                        tab,
                        ..View::default()
                    };
                    let rows = entries(
                        &snapshot,
                        &projection,
                        &agent,
                        &view,
                        &HashMap::new(),
                        false,
                        expanded,
                    );
                    assert_eq!(rows.len(), 3);
                    for (index, target) in
                        ["root", "lab-monitoring", "root"].into_iter().enumerate()
                    {
                        let row = &rows[index];
                        let header = row.text.lines().next().unwrap();
                        let preview = if target == "root" {
                            "exec ls"
                        } else {
                            "exec @lab-monitoring ls"
                        };
                        assert!(header.contains(preview), "{header}");
                        assert!(!header.contains("@root"));
                        assert!(header.contains(state_name(state)));
                        assert_eq!(row.indent, index as u16 * 2);
                        let narrow = super::super::render::wrap_plain(header, 35);
                        assert!(narrow[0].contains(preview));
                        if expanded {
                            assert!(row.text.contains(&format!("{target} · /workspace")));
                        }
                    }
                }
            }
        }
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

    #[test]
    fn request_summaries_show_per_request_tokens_and_recorded_duration() {
        let mut snapshot = ObservationSnapshot::default();
        let agent = AgentId::root(SessionId::from_bytes([49; 16]));
        let context = context(&mut snapshot, &agent);
        let first = request(&mut snapshot, &agent, context);
        snapshot.records.get_mut(&first).unwrap().timestamp_millis = 1_000;
        let response = record(
            &mut snapshot,
            &agent,
            SessionEvent::MessageCommitted {
                message: Message::Assistant(vec![AssistantItem::text("text", 1, "done")]),
            },
        );
        snapshot
            .records
            .get_mut(&response)
            .unwrap()
            .timestamp_millis = 3_500;
        let usage = record(
            &mut snapshot,
            &agent,
            SessionEvent::Usage {
                request: Some(first),
                usage: Usage {
                    input_tokens: 120,
                    cached_input_tokens: 300,
                    output_tokens: 45,
                },
            },
        );
        snapshot.records.get_mut(&usage).unwrap().timestamp_millis = 9_000;
        let second = request(&mut snapshot, &agent, context);
        snapshot.records.get_mut(&second).unwrap().timestamp_millis = 10_000;
        let failure = record(
            &mut snapshot,
            &agent,
            SessionEvent::ModelFailed {
                request: second,
                attempt: 1,
                error: "provider failure".into(),
            },
        );
        snapshot.records.get_mut(&failure).unwrap().timestamp_millis = 11_250;
        let mut projection = Projection::default();
        projection.rebuild(&snapshot);
        let mut view = View {
            tab: Tab::Requests,
            ..View::default()
        };
        for expanded in [false, true] {
            if expanded {
                view.expanded
                    .extend([format!("r{first}"), format!("r{second}")]);
            }
            let rows = entries(
                &snapshot,
                &projection,
                &agent,
                &view,
                &HashMap::new(),
                false,
                false,
            );
            assert!(rows[0].text.contains("Out 45 · In 120 · Cached 300 · 2.5s"));
            assert!(rows[1].text.contains("Out — · In — · Cached — · 1.2s"));
            assert!(rows.iter().all(|row| !row.running));
        }
        assert_eq!(
            request_stats(&RequestInfo::default(), false),
            "Out — · In — · Cached — · Time —"
        );
        let backwards = RequestInfo {
            started_millis: Some(100),
            finished_millis: Some(50),
            ..RequestInfo::default()
        };
        assert!(request_stats(&backwards, false).ends_with("0.0s"));
    }

    #[test]
    fn active_request_elapsed_summary_updates_without_rebuilding_history() {
        let mut snapshot = ObservationSnapshot::default();
        let agent = AgentId::root(SessionId::from_bytes([50; 16]));
        let context = context(&mut snapshot, &agent);
        let id = request(&mut snapshot, &agent, context);
        snapshot
            .activity
            .insert(agent.clone(), AgentActivity::Working);
        let mut projection = Projection::default();
        projection.rebuild(&snapshot);
        let view = View {
            tab: Tab::Requests,
            ..View::default()
        };
        let presentation = EntryView {
            agent: &agent,
            view: &view,
            thinking: false,
            all_details: false,
        };
        let mut cache = ContentCache::default();
        let mut rows = Vec::new();
        let outputs = HashMap::new();
        cache.update(&mut rows, &snapshot, &projection, presentation, &outputs, 0);
        assert!(rows[0].running);
        assert!(rows[0].text.contains("elapsed"));
        // Advance the effective start deterministically rather than sleeping.
        projection.requests.get_mut(&id).unwrap().started_millis = Some(5_000);
        let changes = cache.update(&mut rows, &snapshot, &projection, presentation, &outputs, 0);
        assert!(!changes.reset);
        assert_eq!(changes.dirty, [0]);
        assert!(changes.appends.is_empty());
        assert_eq!(cache.historical_rebuilds, 1);
        assert_eq!(projection.request_reconstructions.get(), 0);
        snapshot
            .activity
            .insert(agent.clone(), AgentActivity::Interrupted);
        let changes = cache.update(&mut rows, &snapshot, &projection, presentation, &outputs, 0);
        assert_eq!(changes.dirty, [0]);
        assert!(!rows[0].running);
        assert!(rows[0].text.contains("Time —"));
    }

    #[test]
    fn expanded_request_is_reconstructed_once_across_stream_updates_and_late_usage() {
        let mut snapshot = ObservationSnapshot::default();
        let agent = AgentId::root(SessionId::from_bytes([2; 16]));
        let context = context(&mut snapshot, &agent);
        let first = request(&mut snapshot, &agent, context);
        for _ in 0..2 {
            request(&mut snapshot, &agent, context);
        }
        let mut projection = Projection::default();
        projection.rebuild(&snapshot);
        let mut view = View {
            tab: Tab::Requests,
            ..View::default()
        };
        let outputs = HashMap::new();
        assert_eq!(
            entries(
                &snapshot,
                &projection,
                &agent,
                &view,
                &outputs,
                false,
                false
            )
            .len(),
            3
        );
        assert_eq!(projection.request_reconstructions.get(), 0);
        view.expanded.insert(format!("r{first}"));
        for _ in 0..3 {
            update(
                &mut snapshot,
                delta_event(agent.clone(), first, BlockKind::Text, "chunk".into()),
            );
            projection.rebuild(&snapshot);
            let entries = entries(
                &snapshot,
                &projection,
                &agent,
                &view,
                &outputs,
                false,
                false,
            );
            assert!(entries[0].text.contains("fixture-model"));
            assert!(entries[0].text.contains("original request"));
        }
        record(
            &mut snapshot,
            &agent,
            SessionEvent::ModelFailed {
                request: first,
                attempt: 1,
                error: "late failure".into(),
            },
        );
        record(
            &mut snapshot,
            &agent,
            SessionEvent::Usage {
                request: Some(first),
                usage: Usage {
                    input_tokens: 12,
                    output_tokens: 3,
                    cached_input_tokens: 0,
                },
            },
        );
        projection.rebuild(&snapshot);
        let entries = entries(
            &snapshot,
            &projection,
            &agent,
            &view,
            &outputs,
            false,
            false,
        );
        assert!(entries[0].text.contains("late failure"));
        assert!(entries[0].text.contains("Usage"));
        assert_eq!(projection.request_reconstructions.get(), 1);
    }

    #[test]
    fn exact_footer() {
        assert_eq!(
            footer(
                Usage {
                    output_tokens: 8400,
                    input_tokens: 31200,
                    cached_input_tokens: 168800
                },
                Some((54000, 128000))
            ),
            "8.4k · 200k(31.2k) · 42% (54k/128k)"
        );
    }
    #[test]
    fn control_characters_cannot_escape_the_ui() {
        assert_eq!(clean("a\x1b[2J\x07b\n"), "a[2Jb\n");
    }
}

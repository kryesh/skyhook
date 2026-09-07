use super::{
    format::{agent_label, brief},
    tool_view::{Document, Role},
};
use serde_json::Value;
use skyhook::{
    agent::{AgentActivity, LiveResponse, ObservationSnapshot},
    identity::{AgentId, JobId},
    job::JobState,
    provider::protocol::{AssistantContent, Message, Usage, UserContent},
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
        !response.settled && !self.response_committed(request)
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
                    self.requests.entry(record.sequence).or_default().model = model;
                    self.active_request
                        .insert(record.agent.clone(), record.sequence);
                }
                SessionEvent::ModelFailed { request, error, .. } => {
                    self.requests.entry(*request).or_default().failed = Some(error.clone());
                }
                SessionEvent::Usage {
                    request: Some(request),
                    usage,
                } => {
                    self.requests
                        .entry(*request)
                        .or_default()
                        .details
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
    footer(
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
/// `revision` for every content mutation other than append-only TextDelta and
/// ReasoningDelta events (including output downloads and presentation changes).
/// Journal progress, selected agent, tab and defaults are also checked here.
///
/// Entries remain caller-owned: neither historical strings nor tool documents
/// are cloned to hand the cache's result to the renderer.
#[derive(Default)]
pub struct ContentCache {
    identity: Option<(AgentId, Tab, bool, bool, u64, u64)>,
    history_len: usize,
    observed_responses: HashMap<AgentId, HashSet<u64>>,
    history_running: bool,
    live: Vec<LiveContent>,
    request: Option<LiveRequestContent>,
    job_indices: HashMap<JobId, usize>,
    invalid_jobs: HashSet<JobId>,
    #[cfg(test)]
    historical_rebuilds: usize,
    #[cfg(test)]
    historical_entries: usize,
}

struct LiveRequestContent {
    request: u64,
    index: usize,
    response_lengths: Option<(usize, usize)>,
}

struct LiveContent {
    request: u64,
    start: usize,
    count: usize,
    text_len: usize,
    reasoning_len: usize,
    reasoning_end: usize,
}

#[derive(Clone, Copy)]
pub struct EntryView<'a> {
    pub agent: &'a AgentId,
    pub view: &'a View,
    pub thinking: bool,
    pub all_details: bool,
}

impl ContentCache {
    /// Deltas may precede a journal request record (including restored streams).
    /// Keep discovery indexed instead of scanning all retained failed responses.
    pub fn observe_response(&mut self, agent: &AgentId, request: u64) {
        self.observed_responses
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
        // intact. Byte comparisons occur only on explicit invalidation, never
        // on the streaming path. Reuse equal allocations, too.
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
            self.observed_responses.remove(agent);
            self.identity = Some(identity);
            *entries = entries_inner(snapshot, projection, presentation, outputs, false);
            self.history_len = entries.len();
            self.history_running = entries.iter().any(|entry| entry.running);
            self.live.clear();
            self.request = None;
            #[cfg(test)]
            {
                self.historical_rebuilds += 1;
                self.historical_entries += entries.len();
            }
            if view.tab == Tab::Requests
                && let Some(request) = projection.active_request.get(agent)
            {
                let key = format!("r{request}");
                if let Some(index) = entries
                    .iter()
                    .position(|e| e.key == key && view.expanded.contains(&key))
                {
                    self.request = Some(LiveRequestContent {
                        request: *request,
                        index,
                        response_lengths: snapshot
                            .responses
                            .get(&(agent.clone(), *request))
                            .map(|r| (r.reasoning.len(), r.text.len())),
                    });
                }
            }
        }
        if !reset {
            for job in self.invalid_jobs.drain() {
                if let Some(&index) = self.job_indices.get(&job)
                    && let Some(info) = projection.jobs.get(&job)
                {
                    entries[index] = job_entry(info, projection, view, outputs, all_details);
                    changes.dirty.push(index);
                }
            }
        }
        if view.tab == Tab::Requests
            && !reset
            && let Some(live) = &mut self.request
            && let Some(response) = snapshot.responses.get(&(agent.clone(), live.request))
        {
            // Preserve the recorded input; only change its response suffix.
            let index = live.index;
            let entry = &mut entries[index];
            if let Some((reasoning_len, text_len)) = live.response_lengths {
                if response.reasoning.len() > reasoning_len {
                    let at = entry.text.len() - text_len - 1;
                    entry
                        .text
                        .insert_str(at, &response.reasoning[reasoning_len..]);
                    changes.dirty.push(index);
                }
                if response.text.len() > text_len {
                    let previous_len = entry.text.len();
                    entry.text.push_str(&response.text[text_len..]);
                    if !changes.dirty.contains(&index) {
                        changes.dirty.push(index);
                        changes.appends.insert(index, previous_len);
                    }
                }
            } else {
                changes.appends.insert(index, entry.text.len());
                entry.text.push_str("\nResponse\n");
                entry.text.push_str(&response.reasoning);
                entry.text.push('\n');
                entry.text.push_str(&response.text);
                changes.dirty.push(index);
            }
            live.response_lengths = Some((response.reasoning.len(), response.text.len()));
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
                    text_len: response.text.len(),
                    reasoning_len: response.reasoning.len(),
                    reasoning_end: response.reasoning.trim_end_matches(['\r', '\n']).len(),
                });
            }
        } else {
            // A request first acquires a response on its first delta. Locate it
            // using the projection index, not by scanning settled responses.
            let mut observed = self.observed_responses.remove(agent).unwrap_or_default();
            if let Some(request) = projection.active_request.get(agent) {
                observed.insert(*request);
            }
            let mut observed: Vec<_> = observed.into_iter().collect();
            observed.sort_unstable();
            for request in &observed {
                if !self.live.iter().any(|live| live.request == *request)
                    && let Some(response) = snapshot.responses.get(&(agent.clone(), *request))
                    && projection.live_response(*request, response)
                {
                    let start = self
                        .live
                        .last()
                        .map_or(self.history_len, |l| l.start + l.count);
                    let replacement =
                        response_entries(*request, response, view, thinking, agent_name);
                    let count = replacement.len();
                    entries.splice(start..start, replacement);
                    changes.dirty.extend(start..entries.len());
                    self.live.push(LiveContent {
                        request: *request,
                        start,
                        count,
                        text_len: response.text.len(),
                        reasoning_len: response.reasoning.len(),
                        reasoning_end: response.reasoning.trim_end_matches(['\r', '\n']).len(),
                    });
                }
            }
            // Only inspect lengths and newly appended suffixes.
            let mut shift = 0isize;
            for live in &mut self.live {
                live.start = live
                    .start
                    .checked_add_signed(shift)
                    .expect("live entry offset");
                let Some(response) = snapshot.responses.get(&(agent.clone(), live.request)) else {
                    continue;
                };
                if response.text.len() == live.text_len
                    && response.reasoning.len() == live.reasoning_len
                {
                    continue;
                }
                let new_reasoning = &response.reasoning[live.reasoning_len..];
                let trimmed_delta = new_reasoning.trim_end_matches(['\r', '\n']);
                let end = if trimmed_delta.is_empty() {
                    live.reasoning_end
                } else {
                    live.reasoning_len + trimmed_delta.len()
                };
                let reasoning_index = (live.count > 0
                    && entries[live.start].surface == Surface::Reasoning)
                    .then_some(live.start);
                // Rebuild just this live response on a structural transition:
                // first answer, first reasoning, or single-line -> disclosure.
                let shape_change = (live.text_len == 0 && !response.text.is_empty())
                    || (reasoning_index.is_none()
                        && new_reasoning.chars().any(|c| !c.is_whitespace()))
                    || reasoning_index.is_some_and(|i| {
                        !entries[i].expandable
                            && response.reasoning[live.reasoning_end..end].contains('\n')
                    });
                if shape_change {
                    let replacement =
                        response_entries(live.request, response, view, thinking, agent_name);
                    let count = replacement.len();
                    entries.splice(live.start..live.start + live.count, replacement);
                    shift += count as isize - live.count as isize;
                    live.count = count;
                    // A structural insertion shifts any later live/status rows.
                    changes.dirty.extend(live.start..entries.len());
                } else {
                    if let Some(index) = reasoning_index
                        && end > live.reasoning_end
                        && (!entries[index].expandable
                            || view.is_expanded(&entries[index].key, entries[index].default_open))
                    {
                        let previous_len = entries[index].text.len();
                        entries[index]
                            .text
                            .push_str(&response.reasoning[live.reasoning_end..end]);
                        changes.dirty.push(index);
                        if shift == 0 {
                            changes.appends.insert(index, previous_len);
                        }
                    }
                    if response.text.len() > live.text_len {
                        let index = live.start + live.count - 1;
                        let previous_len = entries[index].text.len();
                        entries[index]
                            .text
                            .push_str(&response.text[live.text_len..]);
                        changes.dirty.push(index);
                        if shift == 0 {
                            changes.appends.insert(index, previous_len);
                        }
                    }
                }
                live.text_len = response.text.len();
                live.reasoning_len = response.reasoning.len();
                live.reasoning_end = end;
            }
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
                                response.reasoning, response.text
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
                        Message::Assistant(blocks) => {
                            let first_reasoning = blocks.iter().position(|block| matches!(block, AssistantContent::Reasoning { text, .. } if !text.is_empty()));
                            let final_text = if blocks
                                .iter()
                                .any(|block| matches!(block, AssistantContent::ToolCall(_)))
                            {
                                None
                            } else {
                                blocks.iter().rposition(|block| matches!(block, AssistantContent::Text { text } if !text.is_empty()))
                            };
                            for (i, block) in blocks.iter().enumerate() {
                                match block {
                                    AssistantContent::Text { text } if !text.is_empty() => {
                                        let mut entry = Entry::new(
                                            format!("{key}/{i}"),
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
                                    AssistantContent::Reasoning { text, .. }
                                        if !text.trim().is_empty() =>
                                    {
                                        entries.push(reasoning_entry(
                                            // Preserve clicks made during the live-to-journal handoff.
                                            if Some(i) == first_reasoning
                                                && let Some(request) = projection
                                                    .response_requests
                                                    .get(&record.sequence)
                                            {
                                                format!("reasoning-done{request}")
                                            } else {
                                                format!("{key}/{i}")
                                            },
                                            text,
                                            view,
                                            thinking,
                                            "Reasoning",
                                        ));
                                    }
                                    AssistantContent::ToolCall(call) => {
                                        let exists = projection
                                            .tool_origins
                                            .contains(&(record.sequence, call.id.clone()));
                                        if !exists {
                                            let key = format!("{key}/{i}");
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
                    SessionEvent::JobCreated { job, .. } => {
                        if let Some(job) = projection.jobs.get(job) {
                            entries.push(job_entry(job, projection, view, outputs, all_details));
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
        return Entry::new(key, text.to_owned(), Surface::Reasoning);
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

fn response_entries(
    request: u64,
    response: &LiveResponse,
    view: &View,
    thinking: bool,
    agent_name: &str,
) -> Vec<Entry> {
    let mut entries = Vec::new();
    if !response.reasoning.trim().is_empty() {
        // Answer text ends the reasoning phase, even while the response is still live.
        // A separate key prevents a live expansion from overriding auto-collapse.
        let running = !response.settled && response.text.is_empty();
        let phase = if running { "live" } else { "done" };
        let mut entry = reasoning_entry(
            format!("reasoning-{phase}{request}"),
            &response.reasoning,
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
        entries.push(entry);
    }
    if !response.text.is_empty() {
        entries.push(Entry::new(
            format!("live{request}"),
            format!(
                "{}\n{}",
                if response.error.is_some() {
                    "Incomplete response"
                } else {
                    agent_name
                },
                response.text,
            ),
            if response.error.is_some() {
                Surface::Error
            } else {
                Surface::Agent
            },
        ));
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
        provider::protocol::ModelRequest,
        session::{ContextMessage, EventRecord, ModelPurpose},
    };
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
    fn content_cache_streams_without_rebuilding_or_cloning_history() {
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
                RuntimeEvent::TextDelta {
                    agent: agent.clone(),
                    request,
                    text: "chunk".into(),
                },
            );
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
            if n > 0 {
                assert_eq!(changes.appends.get(&100), Some(&("Agent\n".len() + n * 5)));
            }
            assert_eq!(rows[100].text, format!("Agent\n{}", "chunk".repeat(n + 1)));
        }
        // Presentation/output invalidation is explicit and conservative.
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
                RuntimeEvent::ReasoningDelta {
                    agent: agent.clone(),
                    request,
                    text: text.into(),
                },
            );
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
            RuntimeEvent::TextDelta {
                agent: agent.clone(),
                request,
                text: "answer".into(),
            },
        );
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
        assert_eq!(cache.historical_rebuilds, 1);
    }

    #[test]
    fn content_cache_expanded_request_appends_without_reconstructing_input() {
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
                RuntimeEvent::ReasoningDelta {
                    agent: agent.clone(),
                    request,
                    text: text.into(),
                }
            } else {
                RuntimeEvent::TextDelta {
                    agent: agent.clone(),
                    request,
                    text: text.into(),
                }
            };
            update(&mut snapshot, event);
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

    fn update(snapshot: &mut ObservationSnapshot, event: RuntimeEvent) {
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
                    RuntimeEvent::ReasoningDelta {
                        agent: agent.clone(),
                        request,
                        text: text.into(),
                    },
                );
            }
            let live = rows(&snapshot, &view);
            assert_eq!(live.len(), 1);
            assert_eq!(live[0].text, "▾   Reasoning\nFirst step\nSecond step");
            assert!(live[0].expandable && live[0].default_open && live[0].running);
            view.collapsed.insert(live[0].key.clone());
            update(
                &mut snapshot,
                RuntimeEvent::ReasoningDelta {
                    agent: agent.clone(),
                    request,
                    text: "\nThird step".into(),
                },
            );
            assert_eq!(rows(&snapshot, &view)[0].text, "▸   Reasoning");
            view.collapsed.clear();
            view.expanded.insert(live[0].key.clone());
            update(
                &mut snapshot,
                RuntimeEvent::TextDelta {
                    agent: agent.clone(),
                    request,
                    text: "Answer".into(),
                },
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
                        AssistantContent::Reasoning {
                            text: "First step\nSecond step\nThird step".into(),
                            opaque: None,
                        },
                        AssistantContent::Reasoning {
                            text: String::new(),
                            opaque: Some(serde_json::json!({"signature": "opaque"})),
                        },
                        AssistantContent::Text {
                            text: "Answer".into(),
                        },
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
                RuntimeEvent::ReasoningDelta {
                    text: if owner == agent {
                        "Partial reasoning"
                    } else {
                        "Child reasoning"
                    }
                    .into(),
                    agent: owner,
                    request,
                },
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
            RuntimeEvent::ReasoningDelta {
                agent: agent.child(1),
                request,
                text: "Child only".into(),
            },
        );
        assert_eq!(rows(&snapshot)[0].text, "  Working");
        update(
            &mut snapshot,
            RuntimeEvent::ReasoningDelta {
                agent: agent.clone(),
                request,
                text: "One step".into(),
            },
        );
        let reasoning = rows(&snapshot);
        assert_eq!(reasoning.len(), 1, "reasoning replaces the generic spinner");
        assert!(reasoning[0].running && !reasoning[0].expandable);
        update(
            &mut snapshot,
            RuntimeEvent::TextDelta {
                agent: agent.clone(),
                request,
                text: "Answer".into(),
            },
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
                    AssistantContent::Reasoning {
                        text: "One step".into(),
                        opaque: None,
                    },
                    AssistantContent::Text {
                        text: "Answer".into(),
                    },
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
                        AssistantContent::Text {
                            text: "Checking a file".into(),
                        },
                        AssistantContent::ToolCall(skyhook::provider::protocol::ToolCall {
                            id: format!("call-{name}"),
                            name: "read".into(),
                            arguments: serde_json::json!({"path":"file"}),
                        }),
                    ]),
                },
            );
            request(&mut snapshot, &agent, context);
            record(
                &mut snapshot,
                &agent,
                SessionEvent::MessageCommitted {
                    message: Message::Assistant(vec![
                        AssistantContent::Text {
                            text: "Answer part one".into(),
                        },
                        AssistantContent::Text {
                            text: "Answer part two".into(),
                        },
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
                    message: Message::Assistant(vec![AssistantContent::ToolCall(
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
            RuntimeEvent::TextDelta {
                agent: root.clone(),
                request: failed,
                text: "failed partial".into(),
            },
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
                message: Message::Assistant(vec![AssistantContent::Text {
                    text: "successful retry".into(),
                }]),
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
            RuntimeEvent::TextDelta {
                agent: root.clone(),
                request: interrupted,
                text: "interrupted partial".into(),
            },
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
            RuntimeEvent::TextDelta {
                agent: root.clone(),
                request: current,
                text: "current stream".into(),
            },
        );
        update(
            &mut snapshot,
            RuntimeEvent::TextDelta {
                agent: child,
                request: retry,
                text: "child only".into(),
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
                RuntimeEvent::TextDelta {
                    agent: agent.clone(),
                    request: first,
                    text: "chunk".into(),
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

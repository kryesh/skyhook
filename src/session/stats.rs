//! Per-agent and per-model accounting over a session journal: where tokens went,
//! how model requests ended, what was delegated, and which tools were called.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt::Write as _,
};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::{
    identity::{AgentId, JobId, SessionId},
    job::JobRole,
    provider::protocol::{Message, Usage, UserContent},
    session::{EventRecord, ModelPurpose, SessionEvent},
};

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct SessionStats {
    pub session: SessionId,
    /// The first text the user sent the root agent.
    pub initial_prompt: Option<String>,
    pub started: DateTime<Utc>,
    /// The last journal entry.
    pub finished: DateTime<Utc>,
    /// Sorted by path, root first.
    pub agents: Vec<AgentStats>,
    /// Keyed by model profile name.
    pub models: BTreeMap<String, ModelStats>,
    /// Keyed by tool name, summed over every agent.
    pub tools: BTreeMap<String, ToolStats>,
    pub totals: Totals,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct AgentStats {
    /// Unix-like, from agent names: `/` for the root, then `/foo`, `/foo/bar`. A
    /// later sibling with the same name is suffixed with its index: `/foo#2`.
    pub path: String,
    pub name: String,
    pub depth: usize,
    /// The last applied model profile; none for a tool-only agent.
    pub model: Option<String>,
    pub parent: Option<String>,
    pub owner_job: Option<JobId>,
    /// A new model request after any of these makes the agent running again.
    pub outcome: AgentOutcome,
    pub started: DateTime<Utc>,
    /// The agent's final completion, interruption, or failure. Later model requests
    /// clear it, so a resumed child reports only its last one.
    pub finished: Option<DateTime<Utc>>,
    /// Every usage report of the agent, including compaction requests.
    pub usage: Usage,
    pub requests: RequestStats,
    pub compactions: CompactionStats,
    pub tools: BTreeMap<String, ToolStats>,
    /// Jobs this agent created, by role.
    pub jobs: JobCounts,
    pub children: u64,
}

/// Shutdown interrupts every agent, so an interruption counts only for an agent in
/// the middle of a turn; an idle root's shutdown completes it instead.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AgentOutcome {
    #[default]
    Running,
    Completed,
    Interrupted,
    Failed {
        error: String,
    },
}

/// Agent-purpose model requests. A request counts once, by its last outcome; one
/// still open at the end of the journal counts under none of the outcomes.
#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub struct RequestStats {
    pub requested: u64,
    pub completed: u64,
    pub failed: u64,
    pub interrupted: u64,
    /// Every attempt, so `attempts - requested` is the number of retries.
    pub attempts: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub struct CompactionStats {
    pub requested: u64,
    pub completed: u64,
    pub skipped: u64,
    pub failed: u64,
    pub before_tokens: u64,
    pub after_tokens: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub struct ToolStats {
    pub calls: u64,
    pub errors: u64,
    /// Calls the journal holds no result for.
    pub unanswered: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub struct JobCounts {
    pub tools: u64,
    pub agents: u64,
    pub scripts: u64,
    pub questions: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub struct ModelStats {
    pub usage: Usage,
    pub requests: RequestStats,
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub struct Totals {
    pub agents: u64,
    pub usage: Usage,
    pub requests: RequestStats,
    pub compactions: CompactionStats,
    pub tool_calls: ToolStats,
    pub jobs: JobCounts,
}

impl RequestStats {
    fn add(&mut self, other: Self) {
        self.requested += other.requested;
        self.completed += other.completed;
        self.failed += other.failed;
        self.interrupted += other.interrupted;
        self.attempts += other.attempts;
    }
}

impl CompactionStats {
    fn add(&mut self, other: Self) {
        self.requested += other.requested;
        self.completed += other.completed;
        self.skipped += other.skipped;
        self.failed += other.failed;
        self.before_tokens = self.before_tokens.saturating_add(other.before_tokens);
        self.after_tokens = self.after_tokens.saturating_add(other.after_tokens);
    }
}

impl ToolStats {
    fn add(&mut self, other: Self) {
        self.calls += other.calls;
        self.errors += other.errors;
        self.unanswered += other.unanswered;
    }
}

impl JobCounts {
    fn add(&mut self, other: Self) {
        self.tools += other.tools;
        self.agents += other.agents;
        self.scripts += other.scripts;
        self.questions += other.questions;
    }
}

/// `root`, or the agent's child indexes such as `1:2`.
#[must_use]
pub fn agent_label(agent: &AgentId) -> String {
    if agent.path().is_empty() {
        return "root".into();
    }
    let path: Vec<_> = agent.path().iter().map(u32::to_string).collect();
    path.join(":")
}

/// The name of an agent no job named.
fn default_name(agent: &AgentId) -> String {
    if agent.path().is_empty() {
        return "root".into();
    }
    format!("agent {}", agent_label(agent))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Open,
    Completed,
    Failed,
    Interrupted,
}

/// One journaled model request: whose it is, what for, on which profile, and how it ended.
struct Request {
    agent: AgentId,
    purpose: ModelPurpose,
    model: Option<String>,
    outcome: Outcome,
}

/// Summarise a session's journal. The records must be in sequence order.
#[must_use]
pub fn session_stats(session: SessionId, records: &[EventRecord]) -> SessionStats {
    let mut agents: BTreeMap<AgentId, AgentStats> = BTreeMap::new();
    let mut requests: HashMap<u64, Request> = HashMap::new();
    let mut contexts: HashMap<u64, String> = HashMap::new();
    let mut job_names: HashMap<JobId, String> = HashMap::new();
    // Tool calls without a result yet, per agent: call id to tool name.
    let mut open_calls: HashMap<AgentId, HashMap<String, String>> = HashMap::new();
    let mut models: BTreeMap<String, ModelStats> = BTreeMap::new();
    // Agents inside a turn: from a model request until a response ends it or the
    // agent completes or fails.
    let mut in_turn: HashSet<AgentId> = HashSet::new();
    // Committed assistant messages that called tools: their response continues the turn.
    let mut calling_messages: HashSet<u64> = HashSet::new();
    // The root's first user message, whether or not it carried text.
    let mut initial_prompt: Option<Option<String>> = None;
    for record in records {
        let agent = &record.agent;
        match &record.event {
            SessionEvent::MessageCommitted {
                message: Message::User(parts),
            } if agent.path().is_empty() && initial_prompt.is_none() => {
                initial_prompt = Some(parts.iter().find_map(|part| match part {
                    UserContent::Text { text } => Some(text.clone()),
                    _ => None,
                }));
            }
            SessionEvent::AgentStarted {
                owner_job, profile, ..
            } => {
                agents.insert(
                    agent.clone(),
                    AgentStats {
                        name: default_name(agent),
                        depth: agent.depth(),
                        model: profile.as_ref().map(|profile| profile.name.clone()),
                        owner_job: *owner_job,
                        started: time(record.timestamp_millis),
                        ..AgentStats::default()
                    },
                );
            }
            SessionEvent::ModelChanged { profile } => {
                if let Some(stats) = agents.get_mut(agent) {
                    stats.model = Some(profile.name.clone());
                }
            }
            SessionEvent::ModelContext { context } => {
                contexts.insert(record.sequence, context.profile.name.clone());
            }
            SessionEvent::ModelRequested {
                context, purpose, ..
            } => {
                let model = contexts.get(context).cloned();
                if let Some(stats) = agents.get_mut(agent) {
                    stats.outcome = AgentOutcome::Running;
                    stats.finished = None;
                    match purpose {
                        ModelPurpose::Agent => stats.requests.requested += 1,
                        ModelPurpose::Compaction => stats.compactions.requested += 1,
                    }
                }
                if *purpose == ModelPurpose::Agent {
                    in_turn.insert(agent.clone());
                    if let Some(model) = &model {
                        models.entry(model.clone()).or_default().requests.requested += 1;
                    }
                }
                requests.insert(
                    record.sequence,
                    Request {
                        agent: agent.clone(),
                        purpose: *purpose,
                        model,
                        outcome: Outcome::Open,
                    },
                );
            }
            SessionEvent::ModelAttemptStarted { request, .. } => {
                if let Some(open) = requests.get_mut(request) {
                    open.outcome = Outcome::Open;
                    if open.purpose == ModelPurpose::Agent {
                        if let Some(stats) = agents.get_mut(&open.agent) {
                            stats.requests.attempts += 1;
                        }
                        if let Some(model) = &open.model {
                            models.entry(model.clone()).or_default().requests.attempts += 1;
                        }
                    }
                }
            }
            SessionEvent::ModelFailed { request, .. } => {
                settle(&mut requests, *request, Outcome::Failed);
            }
            SessionEvent::ModelAttemptInterrupted { request, .. } => {
                settle(&mut requests, *request, Outcome::Interrupted);
            }
            SessionEvent::ResponseCompleted {
                request, message, ..
            } => {
                settle(&mut requests, *request, Outcome::Completed);
                if !message.is_some_and(|message| calling_messages.contains(&message)) {
                    in_turn.remove(agent);
                }
            }
            SessionEvent::Compaction { checkpoint } => {
                settle(&mut requests, checkpoint.request, Outcome::Completed);
                if let Some(stats) = agents.get_mut(agent) {
                    stats.compactions.completed += 1;
                    stats.compactions.before_tokens = stats
                        .compactions
                        .before_tokens
                        .saturating_add(checkpoint.before_tokens);
                    stats.compactions.after_tokens = stats
                        .compactions
                        .after_tokens
                        .saturating_add(checkpoint.after_tokens);
                }
            }
            SessionEvent::CompactionSkipped { request, .. } => {
                settle(&mut requests, *request, Outcome::Completed);
                if let Some(stats) = agents.get_mut(agent) {
                    stats.compactions.skipped += 1;
                }
            }
            SessionEvent::CompactionFailed { request, .. } => {
                if let Some(request) = request {
                    settle(&mut requests, *request, Outcome::Failed);
                }
                if let Some(stats) = agents.get_mut(agent) {
                    stats.compactions.failed += 1;
                }
            }
            SessionEvent::Usage { request, usage } => {
                let model = request
                    .and_then(|request| requests.get(&request))
                    .and_then(|request| request.model.clone())
                    .or_else(|| agents.get(agent).and_then(|stats| stats.model.clone()));
                if let Some(stats) = agents.get_mut(agent) {
                    stats.usage.accumulate(*usage);
                }
                if let Some(model) = model {
                    models.entry(model).or_default().usage.accumulate(*usage);
                }
            }
            SessionEvent::MessageCommitted {
                message: Message::Assistant(items),
            } => {
                let Some(stats) = agents.get_mut(agent) else {
                    continue;
                };
                for call in items.iter().filter_map(|item| item.tool_call_ref()) {
                    calling_messages.insert(record.sequence);
                    stats.tools.entry(call.name().to_owned()).or_default().calls += 1;
                    open_calls
                        .entry(agent.clone())
                        .or_default()
                        .insert(call.id().to_owned(), call.name().to_owned());
                }
            }
            SessionEvent::MessageCommitted {
                message: Message::Tool(results),
            } => {
                let Some(stats) = agents.get_mut(agent) else {
                    continue;
                };
                for result in results {
                    if let Some(open) = open_calls.get_mut(agent) {
                        open.remove(&result.call_id);
                    }
                    if result.is_error {
                        stats.tools.entry(result.name.clone()).or_default().errors += 1;
                    }
                }
            }
            SessionEvent::JobCreated {
                job, role, name, ..
            } => {
                if let Some(name) = name {
                    job_names.insert(*job, name.clone());
                }
                if let Some(stats) = agents.get_mut(agent) {
                    match role {
                        JobRole::Tool => stats.jobs.tools += 1,
                        JobRole::Agent => stats.jobs.agents += 1,
                        JobRole::Script => stats.jobs.scripts += 1,
                        JobRole::Question => stats.jobs.questions += 1,
                    }
                }
            }
            SessionEvent::AgentCompleted => {
                in_turn.remove(agent);
                finish(&mut agents, record, AgentOutcome::Completed);
            }
            SessionEvent::AgentInterrupted => {
                // Shutdown interrupts every agent: one already finished stays so, an
                // idle root is done, and anything else was cut short.
                let mid_turn = in_turn.remove(agent);
                let unfinished = agents
                    .get(agent)
                    .is_some_and(|stats| stats.outcome == AgentOutcome::Running);
                if mid_turn || unfinished {
                    let outcome = if !mid_turn && agent.path().is_empty() {
                        AgentOutcome::Completed
                    } else {
                        AgentOutcome::Interrupted
                    };
                    finish(&mut agents, record, outcome);
                }
            }
            SessionEvent::AgentFailed { error } => {
                in_turn.remove(agent);
                let error = error.clone();
                finish(&mut agents, record, AgentOutcome::Failed { error });
            }
            _ => {}
        }
    }
    for (agent, open) in open_calls {
        if let Some(stats) = agents.get_mut(&agent) {
            for name in open.into_values() {
                stats.tools.entry(name).or_default().unanswered += 1;
            }
        }
    }
    for request in requests.values() {
        if request.purpose != ModelPurpose::Agent {
            continue;
        }
        let model = request
            .model
            .as_ref()
            .map(|model| &mut models.entry(model.clone()).or_default().requests);
        let agent = agents
            .get_mut(&request.agent)
            .map(|stats| &mut stats.requests);
        for stats in agent.into_iter().chain(model) {
            match request.outcome {
                Outcome::Open => {}
                Outcome::Completed => stats.completed += 1,
                Outcome::Failed => stats.failed += 1,
                Outcome::Interrupted => stats.interrupted += 1,
            }
        }
    }
    let parents: Vec<_> = agents.keys().filter_map(AgentId::parent).collect();
    for parent in parents {
        if let Some(stats) = agents.get_mut(&parent) {
            stats.children += 1;
        }
    }
    let mut tools: BTreeMap<String, ToolStats> = BTreeMap::new();
    let mut totals = Totals::default();
    // Parents sort before their children, so a parent's path is known first.
    let mut paths: HashMap<AgentId, String> = HashMap::new();
    for (agent, stats) in &mut agents {
        if let Some(name) = stats.owner_job.and_then(|job| job_names.get(&job)) {
            stats.name.clone_from(name);
        }
        stats.parent = agent
            .parent()
            .and_then(|parent| paths.get(&parent).cloned());
        stats.path = match &stats.parent {
            None => "/".into(),
            Some(parent) => format!("{}/{}", parent.trim_end_matches('/'), stats.name),
        };
        // Siblings may share a name; the later one carries its child index.
        if paths.values().any(|path| *path == stats.path) {
            let _ = write!(stats.path, "#{}", agent.path().last().unwrap_or(&0));
        }
        paths.insert(agent.clone(), stats.path.clone());
        totals.agents += 1;
        totals.usage.accumulate(stats.usage);
        totals.requests.add(stats.requests);
        totals.compactions.add(stats.compactions);
        totals.jobs.add(stats.jobs);
        for (name, tool) in &stats.tools {
            tools.entry(name.clone()).or_default().add(*tool);
            totals.tool_calls.add(*tool);
        }
    }
    SessionStats {
        session,
        initial_prompt: initial_prompt.flatten(),
        started: time(records.first().map_or(0, |record| record.timestamp_millis)),
        finished: time(records.last().map_or(0, |record| record.timestamp_millis)),
        agents: agents.into_values().collect(),
        models,
        tools,
        totals,
    }
}

fn time(millis: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(millis).unwrap_or_default()
}

fn settle(requests: &mut HashMap<u64, Request>, request: u64, outcome: Outcome) {
    if let Some(open) = requests.get_mut(&request) {
        open.outcome = outcome;
    }
}

fn finish(agents: &mut BTreeMap<AgentId, AgentStats>, record: &EventRecord, outcome: AgentOutcome) {
    if let Some(stats) = agents.get_mut(&record.agent) {
        stats.outcome = outcome;
        stats.finished = Some(time(record.timestamp_millis));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        execution::ExecutionLocation,
        job::{JobRole, JobState},
        provider::protocol::{AssistantItem, HistoryLifetime, StopReason, ToolCall, ToolResult},
        session::{
            ModelContext, ModelFailureKind, ModelPurpose, SessionEvent, fixture,
            fixture::MemorySession,
        },
    };
    use serde_json::json;

    fn usage(input: u64, cached: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            cached_input_tokens: cached,
            output_tokens: output,
        }
    }

    fn context(name: &str) -> SessionEvent {
        let mut profile = fixture::profile();
        profile.name = name.into();
        SessionEvent::ModelContext {
            context: ModelContext {
                purpose: ModelPurpose::Agent,
                profile,
                system: Vec::new(),
                tools: Vec::new(),
                response_schema: None,
            },
        }
    }

    fn requested(context: u64) -> SessionEvent {
        SessionEvent::ModelRequested {
            context,
            history: Vec::new(),
            tail: Vec::new(),
            history_lifetime: HistoryLifetime::default(),
            purpose: ModelPurpose::Agent,
        }
    }

    fn attempt(request: u64, attempt: u64) -> SessionEvent {
        SessionEvent::ModelAttemptStarted { request, attempt }
    }

    fn call(id: &str, position: usize, name: &str) -> AssistantItem {
        AssistantItem::tool_call(
            format!("item-{id}"),
            position,
            ToolCall::new(id, name, json!({})).unwrap(),
        )
    }

    fn result(id: &str, name: &str, is_error: bool) -> Message {
        Message::Tool(vec![ToolResult {
            call_id: id.into(),
            name: name.into(),
            result: json!({}),
            images: Vec::new(),
            is_error,
        }])
    }

    /// Shutdown interrupts every agent: a final outcome stays, an idle root is done,
    /// and an idle child that never completed was cut short. Same-named siblings
    /// get distinct paths.
    #[tokio::test]
    async fn shutdown_keeps_final_outcomes_and_sibling_paths_stay_distinct() {
        let session = MemorySession::new().await;
        let (store, root) = (&session.store, &session.agent);
        let append = |agent: &AgentId, event| {
            let (store, agent) = (store.clone(), agent.clone());
            async move { store.append(agent, event).await.unwrap().sequence }
        };
        let job = |id| JobId::new(id).unwrap();
        for id in [1, 2] {
            append(
                root,
                SessionEvent::JobCreated {
                    job: job(id),
                    parent: None,
                    origin: None,
                    tool: "agent".into(),
                    role: JobRole::Agent,
                    name: Some("worker".into()),
                    arguments: json!({}),
                    output_schema: None,
                    accepts_input: true,
                    background: false,
                    authorization_scope: None,
                    location: ExecutionLocation::root(session.root.path().to_path_buf()),
                },
            )
            .await;
        }
        let first = session.start_child(root, 1, Some(job(1))).await;
        let second = session.start_child(root, 2, Some(job(2))).await;
        let context = append(root, context("big")).await;
        // The root's turn continues through a message that called tools, whatever
        // the stop reason says, and a failure is its final outcome.
        let request = append(root, requested(context)).await;
        append(root, attempt(request, 1)).await;
        let message = append(
            root,
            SessionEvent::MessageCommitted {
                message: Message::Assistant(vec![call("a", 0, "read")]),
            },
        )
        .await;
        append(
            root,
            SessionEvent::ResponseCompleted {
                request,
                attempt: 1,
                message: Some(message),
                stop_reason: StopReason::EndTurn,
            },
        )
        .await;
        let mid_turn = session_stats(store.id(), &store.records().await);
        append(
            root,
            SessionEvent::AgentFailed {
                error: "boom".into(),
            },
        )
        .await;
        // The first child answered but was never completed; the second completed.
        let answered = append(&first, requested(context)).await;
        append(&first, attempt(answered, 1)).await;
        append(
            &first,
            SessionEvent::ResponseCompleted {
                request: answered,
                attempt: 1,
                message: None,
                stop_reason: StopReason::EndTurn,
            },
        )
        .await;
        append(&second, SessionEvent::AgentCompleted).await;
        for agent in [&first, &second, root] {
            append(agent, SessionEvent::AgentInterrupted).await;
        }
        let stats = session_stats(store.id(), &store.records().await);
        let outcome = |index: usize| stats.agents[index].outcome.clone();
        assert_eq!(
            outcome(0),
            AgentOutcome::Failed {
                error: "boom".into()
            }
        );
        assert_eq!(outcome(1), AgentOutcome::Interrupted);
        assert_eq!(outcome(2), AgentOutcome::Completed);
        assert!(stats.agents.iter().all(|agent| agent.finished.is_some()));
        assert_eq!(mid_turn.agents[0].outcome, AgentOutcome::Running);
        assert_eq!(stats.agents[1].path, "/worker");
        assert_eq!(stats.agents[2].path, "/worker#2");
        assert_eq!(stats.agents[0].tools["read"].unanswered, 1);
    }

    #[tokio::test]
    async fn journal_is_summarised_per_agent_model_and_tool() {
        let session = MemorySession::new().await;
        let (store, root) = (&session.store, &session.agent);
        let append = |agent: &AgentId, event| {
            let (store, agent) = (store.clone(), agent.clone());
            async move { store.append(agent, event).await.unwrap().sequence }
        };
        append(
            root,
            SessionEvent::MessageCommitted {
                message: Message::User(vec![UserContent::Text {
                    text: "survey the\n  repo".into(),
                }]),
            },
        )
        .await;
        let context = append(root, context("big")).await;
        let request = append(root, requested(context)).await;
        append(root, attempt(request, 1)).await;
        append(
            root,
            SessionEvent::ModelFailed {
                request,
                attempt: 1,
                error: "flaky".into(),
                kind: ModelFailureKind::Error,
            },
        )
        .await;
        append(root, attempt(request, 2)).await;
        let message = append(
            root,
            SessionEvent::MessageCommitted {
                message: Message::Assistant(vec![call("a", 0, "read"), call("b", 1, "exec")]),
            },
        )
        .await;
        append(
            root,
            SessionEvent::ResponseCompleted {
                request,
                attempt: 2,
                message: Some(message),
                stop_reason: StopReason::ToolUse,
            },
        )
        .await;
        append(
            root,
            SessionEvent::Usage {
                request: Some(request),
                usage: usage(100, 40, 7),
            },
        )
        .await;
        append(
            root,
            SessionEvent::MessageCommitted {
                message: result("a", "read", true),
            },
        )
        .await;
        let job = JobId::new(1).unwrap();
        append(
            root,
            SessionEvent::JobCreated {
                job,
                parent: None,
                origin: None,
                tool: "agent".into(),
                role: JobRole::Agent,
                name: Some("worker".into()),
                arguments: json!({}),
                output_schema: None,
                accepts_input: true,
                background: false,
                authorization_scope: None,
                location: ExecutionLocation::root(session.root.path().to_path_buf()),
            },
        )
        .await;
        let child = session.start_child(root, 1, Some(job)).await;
        let open = append(&child, requested(context)).await;
        append(&child, attempt(open, 1)).await;
        append(
            &child,
            SessionEvent::ModelAttemptInterrupted {
                request: open,
                attempt: 1,
            },
        )
        .await;
        append(
            &child,
            SessionEvent::Usage {
                request: None,
                usage: usage(5, 0, 1),
            },
        )
        .await;
        // A completed child is retained: a shutdown-time interruption does not
        // change its outcome, but its next request makes it running again.
        append(&child, SessionEvent::AgentCompleted).await;
        append(&child, SessionEvent::AgentInterrupted).await;
        let retained = session_stats(store.id(), &store.records().await);
        assert_eq!(retained.agents[1].outcome, AgentOutcome::Completed);
        let completed = retained.agents[1].finished.unwrap();
        append(&child, requested(context)).await;
        let resumed = session_stats(store.id(), &store.records().await);
        assert_eq!(resumed.agents[1].outcome, AgentOutcome::Running);
        assert_eq!(resumed.agents[1].finished, None);
        append(&child, SessionEvent::AgentInterrupted).await;
        append(
            root,
            SessionEvent::JobFinished {
                job,
                state: JobState::Interrupted,
                diagnostic: None,
                output_diagnostic: None,
                images: Vec::new(),
            },
        )
        .await;
        // The root never completes: once a response ends its turn, the shutdown
        // interruption reports it as done.
        let ended = append(root, requested(context)).await;
        append(root, attempt(ended, 1)).await;
        append(
            root,
            SessionEvent::ResponseCompleted {
                request: ended,
                attempt: 1,
                message: None,
                stop_reason: StopReason::EndTurn,
            },
        )
        .await;
        append(root, SessionEvent::AgentInterrupted).await;

        let records = store.records().await;
        let stats = session_stats(store.id(), &records);
        assert_eq!(stats.initial_prompt.as_deref(), Some("survey the\n  repo"));
        assert_eq!(stats.agents.len(), 2);
        let (root_stats, child_stats) = (&stats.agents[0], &stats.agents[1]);
        assert_eq!(
            (root_stats.path.as_str(), root_stats.name.as_str()),
            ("/", "root")
        );
        assert_eq!(
            (child_stats.path.as_str(), child_stats.name.as_str()),
            ("/worker", "worker")
        );
        assert_eq!(child_stats.parent.as_deref(), Some("/"));
        assert_eq!((root_stats.depth, child_stats.depth), (0, 1));
        assert_eq!(child_stats.owner_job, Some(job));
        assert_eq!(root_stats.children, 1);
        assert_eq!(root_stats.model.as_deref(), Some("test"));
        assert_eq!(root_stats.outcome, AgentOutcome::Completed);
        assert_eq!(child_stats.outcome, AgentOutcome::Interrupted);
        assert!(child_stats.finished.unwrap() >= completed);
        assert!(child_stats.started >= stats.started && stats.finished >= root_stats.started);
        assert_eq!(
            root_stats.requests,
            RequestStats {
                requested: 2,
                completed: 2,
                failed: 0,
                interrupted: 0,
                attempts: 3,
            }
        );
        assert_eq!(
            (
                child_stats.requests.requested,
                child_stats.requests.interrupted
            ),
            (2, 1)
        );
        assert_eq!(stats.models["big"].requests.requested, 4);
        assert_eq!(root_stats.usage, usage(100, 40, 7));
        assert_eq!(root_stats.jobs.agents, 1);
        let read = root_stats.tools["read"];
        let exec = root_stats.tools["exec"];
        assert_eq!((read.calls, read.errors, read.unanswered), (1, 1, 0));
        assert_eq!((exec.calls, exec.errors, exec.unanswered), (1, 0, 1));
        // The request's context names the profile, not the agent's current selection.
        assert_eq!(stats.models["big"].usage, usage(100, 40, 7));
        assert_eq!(stats.models["big"].requests.completed, 2);
        assert_eq!(stats.models["big"].requests.attempts, 4);
        assert_eq!(stats.models["test"].usage, usage(5, 0, 1));
        assert_eq!(stats.totals.usage, usage(105, 40, 8));
        assert_eq!(stats.totals.tool_calls.calls, 2);
        assert_eq!(stats.tools.len(), 2);
        assert_eq!(stats.totals.agents, 2);
        let json = serde_json::to_value(&stats).unwrap();
        assert_eq!(json["agents"][1]["outcome"]["state"], "interrupted");
        assert_eq!(json["agents"][0]["tools"]["read"]["errors"], 1);
    }
}

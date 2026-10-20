//! Per-agent and per-model accounting over a session journal: where tokens went,
//! how model requests ended, what was delegated, and which tools were called. Each
//! is an aggregate the journal's indexes read, bounded in memory however long it is.
use std::{
    collections::{BTreeMap, HashMap},
    fmt::Write as _,
};

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::{
    EntryKind, ModelPurpose, RequestPhase, RequestRecord, SessionError, agent_state::Outcome, db,
};
use crate::{
    agent::Failure,
    identity::{AgentId, JobId, SessionId},
    provider::{profile::ModelRef, protocol::UsageTotals},
};

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct SessionStats {
    pub session: SessionId,
    /// The first text the user sent the root agent.
    pub initial_prompt: Option<String>,
    pub started: DateTime<Utc>,
    /// The latest journal entry.
    pub finished: DateTime<Utc>,
    /// Sorted by path, root first.
    pub agents: Vec<AgentStats>,
    pub models: BTreeMap<ModelRef, ModelStats>,
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
    /// The last applied model.
    pub model: Option<ModelRef>,
    pub parent: Option<String>,
    pub owner_job: Option<JobId>,
    /// The agent's final completion, interruption, or failure, with its time. A later
    /// model request makes the agent running again, so a resumed child reports only
    /// its last one.
    pub outcome: AgentOutcome,
    pub started: DateTime<Utc>,
    /// Every usage report of the agent, including compaction requests.
    pub usage: UsageTotals,
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
    Completed {
        at: DateTime<Utc>,
    },
    Interrupted {
        at: DateTime<Utc>,
    },
    Failed {
        at: DateTime<Utc>,
        failure: Failure,
    },
}

impl AgentOutcome {
    /// How an agent's work last `ended`, journaled at `millis`.
    pub(crate) fn of(ended: Outcome, millis: i64) -> Self {
        let at = time(millis);
        match ended {
            Outcome::Completed => Self::Completed { at },
            Outcome::Interrupted => Self::Interrupted { at },
            Outcome::Failed(failure) => Self::Failed { at, failure },
        }
    }

    pub fn finished_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Running => None,
            Self::Completed { at } | Self::Interrupted { at } | Self::Failed { at, .. } => {
                Some(*at)
            }
        }
    }
}

/// Agent-purpose model requests. A request counts once, by its last outcome; one
/// still open at the end of the journal counts under none of the outcomes, and one
/// whose agent was interrupted while it was pending counts as interrupted.
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
    pub failed: u64,
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
    pub usage: UsageTotals,
    pub requests: RequestStats,
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub struct Totals {
    pub agents: u64,
    pub usage: UsageTotals,
    pub requests: RequestStats,
    pub compactions: CompactionStats,
    pub tool_calls: ToolStats,
    pub jobs: JobCounts,
}

impl RequestStats {
    /// Count `request` by its last outcome.
    fn count(&mut self, request: &RequestRecord) {
        self.requested += 1;
        self.attempts += request.attempts;
        match request.phase {
            RequestPhase::Requested | RequestPhase::Open { .. } | RequestPhase::Retrying { .. } => {
            }
            RequestPhase::Completed { .. } => self.completed += 1,
            RequestPhase::Failed { .. } => self.failed += 1,
            RequestPhase::Interrupted { .. } => self.interrupted += 1,
        }
    }

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
        self.failed += other.failed;
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

impl super::SessionStore {
    /// Summarise a session without taking its lock; a live owner may keep writing.
    /// It reads the whole journal by aggregate, a page of requests at a time.
    pub async fn stats(
        root: &std::path::Path,
        id: SessionId,
    ) -> Result<SessionStats, SessionError> {
        Self::read_only(root, id, move |db| read(db, id)).await
    }

    #[cfg(test)]
    async fn read_stats(&self) -> SessionStats {
        let id = self.id();
        self.snapshot(move |db| Ok(read(db, id)?)).await.unwrap()
    }
}

fn read(db: &db::Db, session: SessionId) -> Result<SessionStats, db::DbError> {
    let rows = db::agent_paths(db, session, db::Scope::All)?;
    let mut starts = db::stats::starts(db)?;
    let mut agents: BTreeMap<AgentId, AgentStats> = BTreeMap::new();
    let mut models: BTreeMap<ModelRef, ModelStats> = BTreeMap::new();
    for (&row, agent) in &rows {
        let Some(start) = starts.remove(&row) else {
            continue;
        };
        let mut stats = AgentStats {
            name: start.name.unwrap_or_else(|| default_name(agent)),
            depth: agent.depth(),
            model: Some(start.model),
            owner_job: start.owner_job,
            started: time(start.started_millis),
            outcome: start.outcome,
            ..AgentStats::default()
        };
        db::stats::each_request(db, agent, |_, request| {
            // The request's context names the profile, not the agent's current selection.
            let model = models.entry(request.profile.name.clone()).or_default();
            model.usage.add(request.usage);
            stats.usage.add(request.usage);
            match request.purpose {
                ModelPurpose::Compaction => stats.compactions.requested += 1,
                ModelPurpose::Agent => {
                    for counts in [&mut stats.requests, &mut model.requests] {
                        counts.count(&request);
                    }
                }
            }
        })?;
        stats.tools = db::stats::calls(db, row)?;
        stats.jobs = db::stats::jobs(db, row)?;
        stats.compactions.completed = db::stats::count(db, row, EntryKind::Compaction)?;
        stats.compactions.failed = db::stats::count(db, row, EntryKind::CompactionFailed)?;
        agents.insert(agent.clone(), stats);
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
        totals.usage.merge(stats.usage);
        totals.requests.add(stats.requests);
        totals.compactions.add(stats.compactions);
        totals.jobs.add(stats.jobs);
        for (name, tool) in &stats.tools {
            tools.entry(name.clone()).or_default().add(*tool);
            totals.tool_calls.add(*tool);
        }
    }
    // Activation and closing date what they settle at the last activity.
    let (started, finished) = db::stats::span(db)?;
    Ok(SessionStats {
        session,
        initial_prompt: db::root_prompt(db)?,
        started: time(started),
        finished: time(finished),
        agents: agents.into_values().collect(),
        models,
        tools,
        totals,
    })
}

fn time(millis: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(millis).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::protocol::Usage;
    use crate::tests::TestRuntime;
    use crate::{
        execution::ExecutionLocation,
        job::{JobEnd, JobRole},
        provider::protocol::{AssistantItem, ToolCall, ToolResult},
        session::{
            AttemptRef, CompletedOutcome, Message, MessageSeq, ModelContext, ModelPurpose,
            RequestSeq, SessionEvent, UserPart,
            tests::{self, attempt, requested, usage},
        },
    };
    use serde_json::json;

    /// Journal job `index`, a delegation named `worker`, and start the root's child
    /// `index` under it.
    async fn worker(session: &TestRuntime, index: u32) -> AgentId {
        let job = JobId::new(index.into()).unwrap();
        let workspace = session.root.path();
        let created = SessionEvent::JobCreated {
            job,
            parent: None,
            origin: None,
            tool: "agent".into(),
            role: JobRole::Agent,
            name: Some("worker".parse().unwrap()),
            arguments: json!({}),
            output_schema: None,
            accepts_input: true,
            background: false,
            location: ExecutionLocation::root(workspace.to_path_buf()),
        };
        let (store, root) = (&session.store, &session.agent);
        store.append(root.clone(), created).await.unwrap();
        tests::start_child(store, root, index, Some(job), workspace).await
    }

    fn totals(input: u64, cached: u64, output: u64) -> UsageTotals {
        UsageTotals {
            input_tokens: input,
            cached_input_tokens: cached,
            cache_write_input_tokens: 0,
            output_tokens: output,
        }
    }

    fn context_event(name: &str) -> SessionEvent {
        let mut profile = tests::profile();
        profile.name = format!("test/{name}").parse().unwrap();
        SessionEvent::ModelContext {
            context: ModelContext::test(ModelPurpose::Agent, profile),
        }
    }

    fn completed(
        request: RequestSeq,
        attempt: u64,
        message: MessageSeq,
        outcome: CompletedOutcome,
    ) -> SessionEvent {
        SessionEvent::ResponseCompleted {
            attempt: AttemptRef { request, attempt },
            message,
            outcome,
        }
    }

    fn reply(text: &str) -> SessionEvent {
        SessionEvent::MessageCommitted {
            message: Message::Assistant(vec![AssistantItem::text("answer", 0, text)]),
        }
    }

    fn call(id: &str, position: u32, name: &str) -> AssistantItem {
        AssistantItem::tool_call(
            format!("item-{id}"),
            position,
            ToolCall::new(id, name, json!({})).unwrap(),
        )
    }

    fn result(id: &str, name: &str, is_error: bool) -> Message {
        Message::Tool(ToolResult {
            call_id: id.into(),
            name: name.into(),
            result: json!({}),
            images: Vec::new(),
            is_error,
        })
    }

    /// Shutdown interrupts every agent: a final outcome stays, an idle root is done,
    /// and an idle child that never completed was cut short. Same-named siblings
    /// get distinct paths.
    #[tokio::test]
    async fn shutdown_keeps_final_outcomes_and_sibling_paths_stay_distinct() {
        let session = TestRuntime::new().await;
        let (store, root) = (&session.store, &session.agent);
        let append = |agent: &AgentId, event| {
            let (store, agent) = (store.clone(), agent.clone());
            async move { store.append(agent, event).await.unwrap().sequence }
        };
        let first = worker(&session, 1).await;
        let second = worker(&session, 2).await;
        let context = append(root, context_event("big")).await;
        // The root's turn continues through a response that called tools, and a
        // failure is its final outcome.
        let request = append(root, requested(context)).await.request();
        append(root, attempt(request, 1)).await;
        let message = append(
            root,
            SessionEvent::MessageCommitted {
                message: Message::Assistant(vec![call("a", 0, "read")]),
            },
        )
        .await
        .message();
        let tool_use = completed(request, 1, message, CompletedOutcome::ToolUse);
        append(root, tool_use).await;
        let mid_turn = store.read_stats().await;
        append(
            root,
            SessionEvent::AgentFailed {
                failure: Failure::Other("boom".into()),
            },
        )
        .await;
        // The first child answered but was never completed; the second completed.
        let child_context = append(&first, context_event("big")).await;
        let request = append(&first, requested(child_context)).await.request();
        append(&first, attempt(request, 1)).await;
        let message = append(&first, reply("answered")).await.message();
        append(
            &first,
            completed(request, 1, message, CompletedOutcome::Answer),
        )
        .await;
        append(&second, SessionEvent::AgentCompleted).await;
        for agent in [&first, &second, root] {
            append(agent, SessionEvent::AgentInterrupted).await;
        }
        let stats = store.read_stats().await;
        let outcome = |index: usize| stats.agents[index].outcome.clone();
        let boom = Failure::Other("boom".into());
        assert!(matches!(outcome(0), AgentOutcome::Failed { failure, .. } if failure == boom));
        assert!(matches!(outcome(1), AgentOutcome::Interrupted { .. }));
        assert!(matches!(outcome(2), AgentOutcome::Completed { .. }));
        assert!(
            stats
                .agents
                .iter()
                .all(|agent| agent.outcome.finished_at().is_some())
        );
        assert_eq!(mid_turn.agents[0].outcome, AgentOutcome::Running);
        assert_eq!(stats.agents[1].path, "/worker");
        assert_eq!(stats.agents[2].path, "/worker#2");
        assert_eq!(stats.agents[0].tools["read"].unanswered, 1);
    }

    #[tokio::test]
    async fn journal_is_summarised_per_agent_model_and_tool() {
        let session = TestRuntime::new().await;
        let (store, root) = (&session.store, &session.agent);
        let append = |agent: &AgentId, event| {
            let (store, agent) = (store.clone(), agent.clone());
            async move { store.append(agent, event).await.unwrap().sequence }
        };
        append(
            root,
            SessionEvent::MessageCommitted {
                message: Message::User(vec![UserPart::Text {
                    text: "survey the\n  repo".into(),
                }]),
            },
        )
        .await;
        let context = append(root, context_event("big")).await;
        let request = append(root, requested(context)).await.request();
        append(root, attempt(request, 1)).await;
        append(
            root,
            SessionEvent::ModelFailed {
                attempt: AttemptRef {
                    request,
                    attempt: 1,
                },
                failure: crate::session::AttemptFailure::Final(Failure::Other("flaky".into())),
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
        .await
        .message();
        append(
            root,
            completed(request, 2, message, CompletedOutcome::ToolUse),
        )
        .await;
        append(
            root,
            SessionEvent::Usage {
                request,
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
        let child = worker(&session, 1).await;
        let child_context = append(&child, context_event("big")).await;
        let open = append(&child, requested(child_context)).await.request();
        append(&child, attempt(open, 1)).await;
        append(
            &child,
            SessionEvent::ModelAttemptInterrupted(AttemptRef {
                request: open,
                attempt: 1,
            }),
        )
        .await;
        append(
            &child,
            SessionEvent::Usage {
                request: open,
                // Unreported counts add nothing to the totals.
                usage: Usage {
                    input_tokens: Some(5),
                    output_tokens: Some(1),
                    ..Usage::default()
                },
            },
        )
        .await;
        // A completed child is retained: a shutdown-time interruption does not
        // change its outcome, but its next request makes it running again.
        append(&child, SessionEvent::AgentCompleted).await;
        append(&child, SessionEvent::AgentInterrupted).await;
        let retained = store.read_stats().await;
        let completed_at = match retained.agents[1].outcome {
            AgentOutcome::Completed { at } => at,
            ref outcome => panic!("retained child reported {outcome:?}"),
        };
        append(&child, requested(child_context)).await;
        let resumed = store.read_stats().await;
        assert_eq!(resumed.agents[1].outcome, AgentOutcome::Running);
        append(&child, SessionEvent::AgentInterrupted).await;
        append(
            root,
            SessionEvent::JobFinished {
                job,
                state: JobEnd::Interrupted,
                diagnostic: None,
                images: Vec::new(),
            },
        )
        .await;
        let open_call = store.read_stats().await;
        let exec = open_call.agents[0].tools["exec"];
        assert_eq!((exec.calls, exec.errors, exec.unanswered), (1, 0, 1));
        // Its next request follows a result for every call.
        append(
            root,
            SessionEvent::MessageCommitted {
                message: result("b", "exec", false),
            },
        )
        .await;
        // The root never completes: once a response ends its turn, the shutdown
        // interruption reports it as done.
        let ended = append(root, requested(context)).await.request();
        append(root, attempt(ended, 1)).await;
        let message = append(root, reply("done")).await.message();
        append(root, completed(ended, 1, message, CompletedOutcome::Answer)).await;
        append(root, SessionEvent::AgentInterrupted).await;

        let stats = store.read_stats().await;
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
        let (current, big): (ModelRef, ModelRef) =
            ("test/test".parse().unwrap(), "test/big".parse().unwrap());
        assert_eq!(root_stats.model.as_ref(), Some(&current));
        assert!(matches!(root_stats.outcome, AgentOutcome::Completed { .. }));
        assert!(
            matches!(child_stats.outcome, AgentOutcome::Interrupted { at } if at >= completed_at)
        );
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
        // Both child requests were cut short: one mid-attempt, one before any attempt.
        assert_eq!(
            (
                child_stats.requests.requested,
                child_stats.requests.interrupted
            ),
            (2, 2)
        );
        assert_eq!(stats.models[&big].requests.requested, 4);
        assert_eq!(root_stats.usage, totals(100, 40, 7));
        assert_eq!(root_stats.jobs.agents, 1);
        let read = root_stats.tools["read"];
        let exec = root_stats.tools["exec"];
        assert_eq!((read.calls, read.errors, read.unanswered), (1, 1, 0));
        assert_eq!((exec.calls, exec.errors, exec.unanswered), (1, 0, 0));
        // The request's context names the profile, not the agent's current selection.
        assert_eq!(stats.models.keys().collect::<Vec<_>>(), [&big]);
        assert_eq!(stats.models[&big].usage, totals(105, 40, 8));
        assert_eq!(stats.models[&big].requests.completed, 2);
        assert_eq!(stats.models[&big].requests.attempts, 4);
        assert_eq!(stats.totals.usage, totals(105, 40, 8));
        assert_eq!(stats.totals.tool_calls.calls, 2);
        assert_eq!(stats.tools.len(), 2);
        assert_eq!(stats.totals.agents, 2);
        let json = serde_json::to_value(&stats).unwrap();
        assert_eq!(json["agents"][1]["outcome"]["state"], "interrupted");
        assert_eq!(json["agents"][0]["tools"]["read"]["errors"], 1);
    }
}

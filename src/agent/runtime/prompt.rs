use std::path::Path;

use chrono::Local;
use serde::Serialize;

use crate::{
    identity::AgentId,
    job::JobManager,
    provider::protocol::{Message, SystemSegment, UserContent},
    remote::protocol::RemoteClock,
};

pub(super) const BASE_PROMPT: &str = "You are an agent running in Skyhook, a general-purpose tool and orchestration harness. Complete the user's task using the available tools. Prefer direct tool calls for straightforward operations and issue independent calls together. Use `script` when JavaScript control flow, transformation, or dynamic or bounded concurrency is useful. Before finishing, account for relevant active jobs and child questions in the Skyhook runtime state, deciding whether each should be awaited, cancelled, or left running based on the task.";

#[derive(Serialize)]
struct SkyhookContext<'a> {
    workspace: &'a str,
    target: TargetContext<'a>,
    agent: AgentContext,
}

#[derive(Serialize)]
struct TargetContext<'a> {
    name: &'a str,
    kind: &'a str,
}

#[derive(Serialize)]
struct AgentContext {
    role: &'static str,
    depth: usize,
    max_depth: usize,
}

#[derive(Serialize)]
struct SkyhookState {
    date: String,
    timezone: String,
    utc_offset: String,
    active_jobs: Vec<ActiveJob>,
}

#[derive(Serialize)]
struct ActiveJob {
    job: u64,
    tool: String,
    state: crate::job::JobState,
}

pub(super) fn base_segment() -> SystemSegment {
    SystemSegment {
        text: BASE_PROMPT.to_owned(),
        cache: true,
    }
}

pub(super) fn context_segment(
    agent: &AgentId,
    target: &str,
    target_kind: &str,
    workspace: &Path,
    max_depth: usize,
) -> SystemSegment {
    let workspace = workspace.to_string_lossy();
    let context = SkyhookContext {
        workspace: &workspace,
        target: TargetContext {
            name: target,
            kind: target_kind,
        },
        agent: AgentContext {
            role: if agent.depth() == 0 { "root" } else { "child" },
            depth: agent.depth(),
            max_depth,
        },
    };
    SystemSegment {
        text: format!(
            "<skyhook_context>\n{}\n</skyhook_context>",
            serde_json::to_string(&context).expect("Skyhook context is serializable")
        ),
        cache: true,
    }
}

pub(super) async fn state_message(
    jobs: &JobManager,
    agent: &AgentId,
    remote_clock: Option<RemoteClock>,
) -> Message {
    let local = Local::now();
    let (date, timezone, utc_offset) = remote_clock.map_or_else(
        || {
            (
                local.format("%Y-%m-%d").to_string(),
                iana_time_zone::get_timezone().unwrap_or_else(|_| local.format("%Z").to_string()),
                local.format("%:z").to_string(),
            )
        },
        |clock| (clock.date, clock.timezone, clock.utc_offset),
    );
    let active_jobs = jobs
        .list(agent)
        .await
        .into_iter()
        .filter(|job| !job.state.is_terminal())
        .map(|job| ActiveJob {
            job: job.job_id.get(),
            tool: job.tool,
            state: job.state,
        })
        .collect();
    let state = SkyhookState {
        date,
        timezone,
        utc_offset,
        active_jobs,
    };
    Message::User(vec![UserContent::Runtime {
        text: format!(
            "<skyhook_state>\n{}\n</skyhook_state>",
            serde_json::to_string(&state).expect("Skyhook state is serializable")
        ),
    }])
}

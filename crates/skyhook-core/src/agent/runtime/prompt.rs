use std::path::Path;

use chrono::Local;
use serde::Serialize;

use crate::{
    identity::AgentId,
    job::JobManager,
    provider::protocol::{SystemSegment, UserContent},
};

pub(super) const ROOT_PROMPT: &str = "You are an agent running in Skyhook, a general-purpose tool and orchestration harness. Complete the user's task using the available tools. Prefer direct tool calls for straightforward operations and issue independent calls together. Use `script` when JavaScript control flow, transformation, or dynamic or bounded concurrency is useful. Before finishing, account for relevant active jobs and child questions in the Skyhook runtime state, deciding whether each should be awaited, cancelled, or left running based on the task.";
pub(super) const CHILD_PROMPT: &str = "You are an agent running in Skyhook. Complete the task assigned by your parent using the available tools and return the result to the parent. Prefer direct tool calls for straightforward operations and issue independent calls together. Use `script` when JavaScript control flow, transformation, or dynamic or bounded concurrency is useful. Before finishing, account for relevant active jobs in the Skyhook runtime state, deciding whether each should be awaited, cancelled, or left running based on the task.";

#[derive(Serialize)]
struct SkyhookContext<'a> {
    date: String,
    workspace: &'a str,
    target: TargetContext<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<AgentContext>,
}

#[derive(Serialize)]
struct TargetContext<'a> {
    name: &'a str,
    kind: &'a str,
}

#[derive(Serialize)]
struct AgentContext {
    available_depth: usize,
}

#[derive(Serialize)]
struct SkyhookState {
    active_jobs: Vec<ActiveJob>,
}

#[derive(Serialize)]
struct ActiveJob {
    job: u64,
    tool: String,
    state: crate::job::JobState,
}

pub(super) fn system_segment(
    instructions: &[String],
    profile_instructions: Option<&str>,
    agent: &AgentId,
    target: &str,
    target_kind: &str,
    workspace: &Path,
    available_depth: usize,
) -> SystemSegment {
    let workspace = workspace.to_string_lossy();
    let context = SkyhookContext {
        date: Local::now().format("%Y-%m-%d").to_string(),
        workspace: &workspace,
        target: TargetContext {
            name: target,
            kind: target_kind,
        },
        agent: (available_depth > 0).then_some(AgentContext { available_depth }),
    };
    let mut parts = Vec::with_capacity(instructions.len().saturating_add(3));
    parts.push(if agent.depth() == 0 {
        ROOT_PROMPT.to_owned()
    } else {
        CHILD_PROMPT.to_owned()
    });
    parts.extend(instructions.iter().cloned());
    if let Some(instructions) = profile_instructions.filter(|value| !value.is_empty()) {
        parts.push(instructions.to_owned());
    }
    parts.push(format!(
        "<skyhook_context>\n{}\n</skyhook_context>",
        serde_json::to_string(&context).expect("Skyhook context is serializable")
    ));
    SystemSegment {
        text: parts.join("\n\n"),
        cache: true,
    }
}

pub(super) async fn active_jobs_content(jobs: &JobManager, agent: &AgentId) -> Option<UserContent> {
    let active_jobs = jobs
        .list(agent)
        .await
        .into_iter()
        .filter(|job| !job.state.is_terminal())
        .map(|job| ActiveJob {
            job: job.id.get(),
            tool: job.tool,
            state: job.state,
        })
        .collect::<Vec<_>>();
    if active_jobs.is_empty() {
        return None;
    }
    let state = SkyhookState { active_jobs };
    Some(UserContent::Runtime {
        text: format!(
            "<skyhook_state>\n{}\n</skyhook_state>",
            serde_json::to_string(&state).expect("Skyhook state is serializable")
        ),
    })
}

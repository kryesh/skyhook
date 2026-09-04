use chrono::Local;
use serde::Serialize;

use crate::{
    execution::ExecutionLocation,
    identity::AgentId,
    job::JobManager,
    provider::protocol::{SystemSegment, UserContent},
    tool::policy::{Capability, CapabilitySet},
};

pub(super) const ROOT_PROMPT: &str = "You are an agent running in Skyhook, a general-purpose tool and orchestration harness. Complete the user's task using the available tools. Prefer direct tool calls for straightforward operations and issue independent calls together. Use `script` when JavaScript control flow, transformation, or dynamic or bounded concurrency is useful. Before finishing, account for relevant active jobs and child questions in the Skyhook runtime state, deciding whether each should be awaited, cancelled, or left running based on the task.";
pub(super) const CHILD_PROMPT: &str = "You are an agent running in Skyhook. Complete the task assigned by your parent using the available tools and return the result to the parent. Prefer direct tool calls for straightforward operations and issue independent calls together. Use `script` when JavaScript control flow, transformation, or dynamic or bounded concurrency is useful. Before finishing, account for relevant active jobs in the Skyhook runtime state, deciding whether each should be awaited, cancelled, or left running based on the task.";
pub(super) const TARGET_PROMPT: &str = "Target-aware tools follow three selection rules. Omitting `target` runs on the target to which this agent belongs, in this agent's current workspace. Setting `target: \"root\"` runs on the local Skyhook process and host using the root workspace, never SSH. Setting `target` to another named target uses that target's configured workspace and pooled remote shim, establishing the complete configured `via` route after approval if that route has not yet been approved in this session. Explicitly selecting this agent's current named target retains this agent's workspace override. Reconnecting an unchanged route already approved in this session does not request approval again. Prefer named targets and target-aware tools' native `target` parameter over invoking `ssh` manually.";

#[derive(Serialize)]
struct SkyhookContext<'a> {
    date: String,
    workspace: &'a str,
}

#[derive(Serialize)]
struct TargetContext<'a> {
    name: &'a str,
    kind: &'a str,
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
    location: &ExecutionLocation,
    available_depth: usize,
    capabilities: &CapabilitySet,
) -> SystemSegment {
    let workspace = location.workspace.to_string_lossy();
    let context = SkyhookContext {
        date: Local::now().format("%Y-%m-%d").to_string(),
        workspace: &workspace,
    };
    let mut parts = Vec::with_capacity(instructions.len().saturating_add(5));
    parts.push(if agent.depth() == 0 {
        ROOT_PROMPT.to_owned()
    } else {
        CHILD_PROMPT.to_owned()
    });
    for capability in capabilities.iter() {
        if let Some(chunk) = capability_prompt(capability, location, available_depth) {
            parts.push(chunk);
        }
    }
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

fn capability_prompt(
    capability: Capability,
    location: &ExecutionLocation,
    available_depth: usize,
) -> Option<String> {
    match capability {
        Capability::Targets => {
            let target = TargetContext {
                name: &location.target,
                kind: location.kind(),
            };
            Some(format!(
                "{TARGET_PROMPT}\n\n<target_context>\n{}\n</target_context>",
                serde_json::to_string(&target).expect("target context is serializable")
            ))
        }
        Capability::Agents => Some(format!(
            "<agent_context>\n{{\"available_depth\":{available_depth}}}\n</agent_context>"
        )),
        Capability::Read | Capability::Write | Capability::Exec => None,
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

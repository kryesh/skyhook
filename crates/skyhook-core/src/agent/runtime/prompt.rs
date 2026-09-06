use chrono::{DateTime, Local};
use serde::Serialize;

use crate::{
    agent::{TodoItem, todo::TodoStore},
    execution::ExecutionLocation,
    identity::AgentId,
    job::{ActiveJob, JobManager},
    provider::protocol::{SystemSegment, UserContent},
    tool::policy::{Capability, CapabilitySet},
};

pub(super) const ROOT_PROMPT: &str = "You are an agent running in Skyhook. Complete the user's task. Prefer direct tools for simple operations and batch independent calls; use script for JavaScript control flow, transformation or bounded concurrency. Before finishing, account for active jobs and child questions: await, cancel, or intentionally leave running.";
pub(super) const CHILD_PROMPT: &str = "You are an agent running in Skyhook. Complete your parent's task and return the result. Prefer direct tools for simple operations and batch independent calls; use script for JavaScript control flow, transformation or bounded concurrency. Before finishing, await or cancel all owned jobs and descendants. Your job cannot complete while owned work is active; only the root may leave background work running.";
pub(super) const TARGET_PROMPT: &str = r#"Your current target and workspace are in skyhook_context. Tools default to your current target and workspace. A tool's target selects where the operation executes; for command tools, write the command as if already on that machine. The task may concern another machine; when its location is unclear, targets lists available machines without connecting. Findings apply to the target inspected.

"root" always selects the session host—the machine running the main Skyhook process—and its configured session workspace. The "local" type identifies the session host. Selecting a different named target uses that target's configured workspace; selecting your current remote target preserves your workspace override. target_add registers a destination without changing your current target; origin defaults to your current target.

Remote commands receive Skyhook's SSH_AUTH_SOCK. SSH authentication prompts are handled by Skyhook."#;

const WORKSPACE_PROMPT: &str = "A workspace is a base directory, not isolation or a filesystem copy. Paths/cwd accept absolute paths or relative paths including `..`, resolved on the selected machine. Relative child workspace overrides resolve against the selected base.";
const LIFECYCLE_PROMPT: &str = "Model calls return JobView; Result in tool descriptions refers to its result field. JavaScript calls return native results; background launches return job metadata. Some result fields may be truncated. Use job_output to retrieve or filter a field's saved content.\n\nCommand timeouts terminate execution; omitted timeouts have no deadline. Nonzero exit_code is a normal result. Script failures throw with partial error.output. Never circumvent a declined operation; use permitted alternatives or explain the limitation.";
const AGENT_PROMPT: &str = "Children start with fresh history; include the full task, relevant context, and scope. Give children distinct responsibilities and use their results. Consider supplying todos with steps or checkpoints. Agents on one target share its filesystem. Child depth limits further delegation and must be below available_depth.";

const TODO_PROMPT: &str = "Use todo for multi-step work and account for unfinished items.";

#[derive(Serialize)]
struct SkyhookContext<'a> {
    workspace: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<TargetContext<'a>>,
}

#[derive(Serialize)]
struct TargetContext<'a> {
    name: &'a str,
    r#type: crate::target::TargetType,
    host: &'a str,
    origin: &'a str,
    via: Option<&'a str>,
}

#[derive(Serialize)]
struct SkyhookState {
    date: String,
    active_jobs: Vec<ActiveJob>,
    todos: Vec<TodoItem>,
}

pub(super) fn system_segment(
    instructions: &[String],
    profile_instructions: Option<&str>,
    agent: &AgentId,
    location: &ExecutionLocation,
    target: Option<&crate::target::TargetDefinition>,
    available_depth: usize,
    capabilities: &CapabilitySet,
) -> SystemSegment {
    let workspace = location.workspace.to_string_lossy();
    let context = SkyhookContext {
        workspace: &workspace,
        target: capabilities
            .contains(Capability::Targets)
            .then(|| TargetContext {
                name: &location.target,
                r#type: target.map_or(crate::target::TargetType::Local, |target| target.r#type),
                host: target.map_or("localhost", |target| &target.host),
                origin: target.map_or(crate::target::ROOT_TARGET, |target| &target.origin),
                via: target.and_then(|target| target.via.as_deref()),
            }),
    };
    let mut parts = Vec::with_capacity(instructions.len().saturating_add(5));
    parts.push(if agent.depth() == 0 {
        ROOT_PROMPT.to_owned()
    } else {
        CHILD_PROMPT.to_owned()
    });
    parts.push(WORKSPACE_PROMPT.to_owned());
    parts.push(TODO_PROMPT.to_owned());
    parts.push(format!(
        "{LIFECYCLE_PROMPT}\n\nModel result type: JobView = `{}`.",
        crate::tool::job_view_type(capabilities)
    ));
    for capability in capabilities.iter() {
        if let Some(chunk) = capability_prompt(capability, available_depth) {
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

fn capability_prompt(capability: Capability, available_depth: usize) -> Option<String> {
    match capability {
        Capability::Targets => Some(TARGET_PROMPT.to_owned()),
        Capability::Agents => Some(format!(
            "{AGENT_PROMPT}\n\n<agent_context>\n{{\"available_depth\":{available_depth}}}\n</agent_context>"
        )),
        Capability::Read | Capability::Write | Capability::Exec => None,
    }
}

pub(super) async fn runtime_state_content(
    jobs: &JobManager,
    todos: &TodoStore,
    agent: &AgentId,
    capabilities: &CapabilitySet,
) -> UserContent {
    runtime_state_content_at(jobs, todos, agent, capabilities, Local::now()).await
}

pub(super) async fn runtime_state_content_at(
    jobs: &JobManager,
    todos: &TodoStore,
    agent: &AgentId,
    capabilities: &CapabilitySet,
    now: DateTime<Local>,
) -> UserContent {
    let active_jobs = jobs
        .active_states(agent, capabilities, now.timestamp_millis())
        .await;
    let state = SkyhookState {
        date: now.format("%Y-%m-%d").to_string(),
        active_jobs,
        todos: todos
            .inspect(agent, None)
            .await
            .expect("own todo list is always readable")
            .items,
    };
    UserContent::Runtime {
        text: format!(
            "<skyhook_state>\n{}\n</skyhook_state>",
            serde_json::to_string(&state).expect("Skyhook state is serializable")
        ),
    }
}

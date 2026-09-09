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

pub(super) const ROOT_PROMPT: &str = "You are an agent running in Skyhook. Prefer direct tools for simple operations and batch independent calls; use script for JavaScript control flow, transformation or bounded concurrency. Before finishing, wait for needed work, cancel unnecessary work, or explicitly identify jobs left running.";
pub(super) const CHILD_PROMPT: &str = "You are an agent running in Skyhook. Complete your parent's task and return the result. Prefer direct tools for simple operations and batch independent calls; use script for JavaScript control flow, transformation or bounded concurrency. Before finishing, wait for needed work, cancel unnecessary work. You cannot complete while owned work is active.";
pub(super) const TARGET_PROMPT: &str = r#"Tools default to the target and workspace in skyhook_context. A tool's target selects the execution machine; write commands as if already on that machine. If the task's machine is unclear, use targets to discover available machines. Findings apply only to the target inspected.

"root" selects the session host running Skyhook and its configured workspace; "local" identifies that host's target type. Selecting another target uses its configured workspace; selecting the current remote target preserves your workspace override.

Remote commands receive Skyhook's SSH_AUTH_SOCK; Skyhook handles SSH authentication prompts."#;

const WORKSPACE_PROMPT: &str = "Workspaces set the base directory; they do not isolate files. Agents on the same target share its filesystem. Paths/cwd may be absolute or relative (including `..`) on the selected machine. Relative child workspace overrides resolve against the selected base.";
const LIFECYCLE_PROMPT: &str = "Direct tool calls return JobView; Result in tool descriptions refers to its result field. JavaScript calls return native results; background launches return job metadata. Use job_output to retrieve truncated results.\n\nCommand timeouts terminate execution; omitted timeouts have no deadline. Nonzero exit_code is a normal result. Script failures throw with partial error.output. Never circumvent a declined operation; use permitted alternatives or explain the limitation.";
const AGENT_PROMPT: &str = "Children start without your conversation history. Supply their task, relevant context, and scope, and give each child a distinct responsibility.\n\nLet children continue working autonomously. Use the appended runtime state and child messages to decide whether intervention is needed. Elapsed time, a wait timeout, or unchanged turn/tool-call counts alone do not establish that a child is stalled; it may be processing a request or awaiting a tool. When dependent on unfinished work, wait again. Send follow-ups to answer questions, resolve concrete blockers, correct a demonstrated misunderstanding, or communicate changed requirements. Resolve questions from children you supervise. Progress updates do not require a reply. When spawning a child, consider using todos to give it an initial checklist for multi-step work";

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
        "{LIFECYCLE_PROMPT}\n\nDirect tool result type: JobView = `{}`.",
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
    let now = Local::now();
    let items = todos
        .inspect(agent, None)
        .await
        .expect("own todo list is always readable")
        .items;
    runtime_state_with_todos_at(jobs, agent, capabilities, items, now).await
}

/// Preview a candidate compaction's state without publishing its todos.
pub(super) async fn runtime_state_with_todos(
    jobs: &JobManager,
    agent: &AgentId,
    capabilities: &CapabilitySet,
    todos: Vec<TodoItem>,
) -> UserContent {
    runtime_state_with_todos_at(jobs, agent, capabilities, todos, Local::now()).await
}

async fn runtime_state_with_todos_at(
    jobs: &JobManager,
    agent: &AgentId,
    capabilities: &CapabilitySet,
    todos: Vec<TodoItem>,
    now: DateTime<Local>,
) -> UserContent {
    let active_jobs = jobs
        .active_states(agent, capabilities, now.timestamp_millis())
        .await;
    let state = SkyhookState {
        date: now.format("%Y-%m-%d").to_string(),
        active_jobs,
        todos,
    };
    UserContent::Runtime {
        text: format!(
            "<skyhook_state>\n{}\n</skyhook_state>",
            serde_json::to_string(&state).expect("Skyhook state is serializable")
        ),
    }
}

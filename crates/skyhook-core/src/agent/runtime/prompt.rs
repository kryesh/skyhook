use chrono::{DateTime, Local};
use serde::Serialize;

use crate::{
    agent::{TodoItem, todo::TodoStore},
    execution::ExecutionLocation,
    identity::AgentId,
    job::{ActiveJob, JobManager},
    provider::protocol::{Message, SystemSegment, UserContent},
    tool::policy::{Capability, CapabilitySet},
};

pub(super) const ROOT_PROMPT: &str = "You are an agent running in Skyhook. Complete the user's task. Prefer direct tools for simple operations and batch independent calls; use script for JavaScript control flow, transformation or bounded concurrency. Before finishing, account for active jobs and child questions: await, cancel, or intentionally leave running.";
pub(super) const CHILD_PROMPT: &str = "You are an agent running in Skyhook. Complete your parent's task and return the result. Prefer direct tools for simple operations and batch independent calls; use script for JavaScript control flow, transformation or bounded concurrency. Before finishing, await or cancel all owned jobs and descendants. Your job cannot complete while owned work is active; only the root may leave background work running.";
pub(super) const TARGET_PROMPT: &str = r#"Targets: omitted inherits the caller's target/workspace; "root" selects the local host and root workspace (never SSH); another named target uses its configured workspace; explicitly selecting the current named target retains the agent's workspace override. Tools without target selectors inherit. Child workspace overrides the selected base. Prefer named targets/native target arguments over manual SSH."#;

const WORKSPACE_PROMPT: &str = "A workspace is a base directory, not isolation or a filesystem copy. Paths/cwd accept absolute paths or relative paths including `..`, resolved on the selected machine. Relative child workspace overrides resolve against the selected base.";
const LIFECYCLE_PROMPT: &str = "Jobs progress queued → running → completed, optionally via waiting_input → running; completed, failed, cancelled and interrupted are terminal. Background calls and foreground child questions return JobEnvelope. Child question output: {kind:questions,question_id,question_ids,questions:[{id,prompt,options}]}. Questions have no expiry. wait or automatic notifications acknowledge delivery; questions are delivered once, terminal results remain rereadable. inspect can reread delivered questions without acknowledging. A wait timeout returns a nonterminal envelope and leaves work running. cancel includes descendants and managed process groups; confirm terminal state with wait/inspect. Detached processes/unreachable hosts limit cleanup. Command timeouts terminate execution; omitted timeouts have no deadline. Direct failures return errors; script failures throw with partial error.output; nonzero command exit_code is a normal result. Never circumvent a declined operation; use permitted alternatives or explain the limitation.";
const AGENT_PROMPT: &str = "Children have fresh history: include the complete task in prompt. They receive shared harness instructions, the selected profile, tools, targets and host skill catalog; model/profile defaults come from harness configuration. Agents on one target share its filesystem. Child depth is its budget for further generations, defaults to zero and must be less than the caller's available_depth. Children must await/cancel all owned work before completing.";

const TODO_PROMPT: &str = "Use todo for multi-step work; replace the whole ordered list to update it. Multiple items may be in_progress. Account for unfinished items; they do not gate completion. The final skyhook_state supersedes older snapshots. agent.todos seeds pending instructions; children own edits. todo({job:agent_job_id}) reads a descendant's list.";

#[derive(Serialize)]
struct SkyhookContext<'a> {
    workspace: &'a str,
}

#[derive(Serialize)]
struct TargetContext<'a> {
    name: &'a str,
    kind: &'a str,
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
    available_depth: usize,
    capabilities: &CapabilitySet,
) -> SystemSegment {
    let workspace = location.workspace.to_string_lossy();
    let context = SkyhookContext {
        workspace: &workspace,
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
        "{LIFECYCLE_PROMPT}\n\nShared result type: JobEnvelope = `{}`.",
        crate::tool::job_envelope_type(capabilities)
    ));
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

/// Remove only the old, harness-generated active-job snapshot from replayed model history.
/// Durable records and other runtime content (especially notifications) are untouched.
pub(super) fn without_legacy_state(mut message: Message) -> Option<Message> {
    if let Message::User(content) = &mut message {
        content.retain(|item| {
            let UserContent::Runtime { text } = item else {
                return true;
            };
            let Some(body) = text
                .strip_prefix("<skyhook_state>\n")
                .and_then(|text| text.strip_suffix("\n</skyhook_state>"))
            else {
                return true;
            };
            let Ok(serde_json::Value::Object(state)) = serde_json::from_str(body) else {
                return true;
            };
            !(state.len() == 1
                && state
                    .get("active_jobs")
                    .is_some_and(serde_json::Value::is_array))
        });
        if content.is_empty() {
            return None;
        }
    }
    Some(message)
}

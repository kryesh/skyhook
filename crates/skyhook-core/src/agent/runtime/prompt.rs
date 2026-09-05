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

pub(super) const ROOT_PROMPT: &str = "You are an agent running in Skyhook, a general-purpose tool and orchestration harness. Complete the user's task using the available tools. Prefer direct tool calls for straightforward operations and issue independent calls together. Use `script` when JavaScript control flow, transformation, or dynamic or bounded concurrency is useful. Before finishing, account for relevant active jobs and child questions in the Skyhook runtime state, deciding whether each should be awaited, cancelled, or left running based on the task.";
pub(super) const CHILD_PROMPT: &str = "You are an agent running in Skyhook. Complete the task assigned by your parent using the available tools and return the result to the parent. Prefer direct tool calls for straightforward operations and issue independent calls together. Use `script` when JavaScript control flow, transformation, or dynamic or bounded concurrency is useful. Before finishing, await or cancel all owned jobs and descendants. Your agent job cannot complete while owned work remains active; only the root agent may leave background services running after answering.";
pub(super) const TARGET_PROMPT: &str = r#"Workspace operations use these target selection rules:
| Selection | Machine | Base workspace |
|---|---|---|
| Omitted | Calling agent's target | Calling agent's workspace |
| `"root"` | Local Skyhook host, never SSH | Root workspace |
| Another named target | That SSH target | Its configured workspace |
| Explicit current named target | Current SSH target | Agent's workspace override |
A tool without a target selector behaves as though target were omitted. An explicit child workspace overrides the selected base. Prefer named targets and native target arguments over manual SSH."#;

const WORKSPACE_PROMPT: &str = "A workspace is a base directory, not an isolation boundary. File paths and command cwd accept absolute paths and relative paths including .., resolved on the selected machine. Relative child workspace overrides resolve against the base selected by target. A workspace override does not create a filesystem copy.";
const LIFECYCLE_PROMPT: &str = "Jobs normally progress queued → running → completed, optionally through waiting_input → running; failed, cancelled, and interrupted are terminal alternatives. Background calls return a JobEnvelope; a foreground child question also returns a suspended envelope. Its output contains kind=questions, question_id, question_ids, and questions [{id,prompt,options}]. wait returns the next undelivered question or terminal envelope and acknowledges delivery, suppressing duplicate automatic notification. Terminal results remain rereadable; a question is delivered once by wait or automatic notification and remains visible through inspect. A timed-out wait returns a nonterminal envelope and leaves the job running. inspect never acknowledges. Send answers to the stable agent job ID; questions have no automatic expiry. cancel requests cancellation of the job, descendant jobs/agents, and managed command process groups; inspect or wait for terminal confirmation. Deliberately detached processes and unreachable remote hosts limit cleanup. All timeouts are optional; commands without a timeout have no deadline, while an explicit command timeout terminates execution. Direct tool failures return errors; script tool calls throw, retaining partial output on error.output. A nonzero command exit is a normal result with exit_code. If an operation is declined, do not circumvent that decision using another tool or route; continue with permitted alternatives or explain what could not be completed.";
const AGENT_PROMPT: &str = "Children start with fresh conversation history: include the complete task in prompt. They receive shared harness instructions, the selected profile, harness tools and configured targets, and the host-owned skill catalog. Model/profile defaults come from the harness unless overridden. Agents on the same target share its filesystem. depth is the child's budget for further generations, defaults to zero, and must be less than the caller's available_depth.";

const TODO_PROMPT: &str = "Use todo to maintain an ordered advisory checklist for multi-step work. Replace the whole list to update progress; multiple items may be in_progress. Account for unfinished items before finishing, but todos do not gate completion. The final skyhook_state snapshot contains your current todos and active jobs and supersedes older state in tool results. Parents may seed children with agent.todos instruction strings, all initially pending; the child owns subsequent edits. Read a descendant's list with todo({job: agent_job_id}).";

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

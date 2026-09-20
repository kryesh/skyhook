use serde::Serialize;

use crate::{
    execution::ExecutionLocation,
    identity::AgentId,
    provider::protocol::SystemSegment,
    tool::policy::{Capability, CapabilitySet, Mode},
};

/// Guidance shared by the root and child prompts, so the two cannot drift.
macro_rules! tool_guidance {
    () => {
        "Prefer direct tools for simple operations and batch independent calls; use script for JavaScript control flow, transformation or bounded concurrency. Before finishing, wait for needed work, cancel unnecessary work"
    };
}
pub(super) const ROOT_PROMPT: &str = concat!(
    "You are an agent running in Skyhook. ",
    tool_guidance!(),
    ", or explicitly identify jobs left running."
);
pub(super) const CHILD_PROMPT: &str = concat!(
    "You are an agent running in Skyhook. Complete your parent's task and return the result. Your replies go directly to your parent. Stay quiet between meaningful milestones, blockers, and the final result; avoid routine progress narration. ",
    tool_guidance!(),
    ". You cannot complete while owned work is active."
);
pub(super) const TARGET_PROMPT: &str = r#"Tools default to the target and workspace in skyhook_context. A tool's target selects the execution machine; write commands as if already on that machine. If the task's machine is unclear, use targets to discover available machines. Findings apply only to the target inspected.

"root" selects the session host running Skyhook and its configured workspace; "local" identifies that host's target type. Selecting another target uses its configured workspace; selecting the current remote target preserves your workspace override.

Remote commands receive the SSH agent their target's connection forwards in SSH_AUTH_SOCK; Skyhook handles SSH authentication prompts."#;

const WORKSPACE_PROMPT: &str = "Workspaces set the base directory; they do not isolate files. Agents on the same target share its filesystem. Paths/cwd may be absolute or relative (including `..`) on the selected machine. Relative child workspace overrides resolve against the selected base.";
const LIFECYCLE_PROMPT: &str = "Write tool arguments with properties in the order the schema lists them. Direct tool calls return JobView; Result in tool descriptions refers to its result field. JavaScript calls return native results; background launches return job metadata. Use job_output to retrieve truncated results.\n\nCommand timeouts terminate execution; omitted timeouts have no deadline. Nonzero exit_code is a normal result. Script failures throw with partial error.output. Never circumvent a declined operation; use permitted alternatives or explain the limitation.";
const AGENT_PROMPT: &str = "Children start without your conversation history. Supply their task, relevant context, and scope, and give each child a distinct responsibility.\n\nLet children continue working autonomously. Use job status and child messages to decide whether intervention is needed. Elapsed time, a wait timeout, or unchanged turn/tool-call counts alone do not establish that a child is stalled; it may be processing a request or awaiting a tool. When dependent on unfinished work, wait again. Send follow-ups to answer questions, resolve concrete blockers, correct a demonstrated misunderstanding, or communicate changed requirements. Resolve questions from children you supervise. Progress updates do not require a reply. When spawning a child, consider using todos to give it an initial checklist for multi-step work";

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
    via: Option<&'a str>,
    origin: Option<&'a str>,
}

/// Everything an agent's system prompt is derived from.
pub(super) struct PromptInputs<'a> {
    pub instructions: &'a [String],
    pub agent: &'a AgentId,
    pub location: &'a ExecutionLocation,
    /// None on the session host.
    pub target: Option<&'a crate::target::TargetDefinition>,
    pub available_depth: usize,
    /// The agent's mode and its name.
    pub mode: Option<(&'a str, &'a Mode)>,
    pub capabilities: &'a CapabilitySet,
}

pub(super) fn system_segment(inputs: &PromptInputs<'_>) -> SystemSegment {
    let PromptInputs {
        instructions,
        agent,
        location,
        target,
        available_depth,
        mode,
        capabilities,
    } = *inputs;
    let workspace = location.workspace.to_string_lossy();
    let context = SkyhookContext {
        workspace: &workspace,
        target: capabilities
            .contains(Capability::Targets)
            .then(|| TargetContext {
                name: &location.target,
                r#type: target.map_or(crate::target::TargetType::Local, |target| target.r#type),
                host: target.map_or("localhost", |target| &target.host),
                via: target.and_then(|target| target.via.as_deref()),
                origin: target.and_then(|target| target.origin.as_deref()),
            }),
    };
    let role = if agent.depth() == 0 {
        ROOT_PROMPT
    } else {
        CHILD_PROMPT
    };
    let mut parts = vec![
        role.to_owned(),
        WORKSPACE_PROMPT.to_owned(),
        TODO_PROMPT.to_owned(),
        format!(
            "{LIFECYCLE_PROMPT}\n\nDirect tool result type: JobView = `{}`.",
            crate::tool::job_view_type(capabilities)
        ),
    ];
    parts.extend(
        capabilities
            .iter()
            .filter_map(|capability| capability_prompt(capability, available_depth)),
    );
    if let Some((
        name,
        Mode {
            instructions: Some(text),
            ..
        },
    )) = mode
    {
        parts.push(format!("<mode name={name:?}>\n{}\n</mode>", text.trim()));
    }
    parts.extend(instructions.iter().cloned());
    parts.push(format!(
        "<skyhook_context>\n{}\n</skyhook_context>",
        serde_json::to_string(&context).expect("Skyhook context is serializable")
    ));
    SystemSegment {
        text: parts.join("\n\n"),
        cache: true,
    }
}

/// Guidance a capability adds to the prompt; most only shape the tool surface.
fn capability_prompt(capability: Capability, available_depth: usize) -> Option<String> {
    match capability {
        Capability::Targets => Some(TARGET_PROMPT.to_owned()),
        Capability::Agents => Some(format!(
            "{AGENT_PROMPT}\n\n<agent_context>\n{{\"available_depth\":{available_depth}}}\n</agent_context>"
        )),
        _ => None,
    }
}

use std::fmt::Write;

use chrono::{DateTime, Local};
use serde::Serialize;

use crate::{
    agent::{TodoItem, TodoStatus, todo::TodoStore},
    execution::ExecutionLocation,
    identity::{AgentId, JobId},
    job::{ActiveJob, JobManager},
    provider::protocol::{SystemSegment, UserContent},
    tool::policy::{Capability, CapabilitySet},
};

pub(super) const ROOT_PROMPT: &str = "You are an agent running in Skyhook. Prefer direct tools for simple operations and batch independent calls; use script for JavaScript control flow, transformation or bounded concurrency. Before finishing, wait for needed work, cancel unnecessary work, or explicitly identify jobs left running.";
pub(super) const CHILD_PROMPT: &str = "You are an agent running in Skyhook. Complete your parent's task and return the result. Your replies go directly to your parent. Stay quiet between meaningful milestones, blockers, and the final result; avoid routine progress narration. Prefer direct tools for simple operations and batch independent calls; use script for JavaScript control flow, transformation or bounded concurrency. Before finishing, wait for needed work, cancel unnecessary work. You cannot complete while owned work is active.";
pub(super) const TARGET_PROMPT: &str = r#"Tools default to the target and workspace in skyhook_context. A tool's target selects the execution machine; write commands as if already on that machine. If the task's machine is unclear, use targets to discover available machines. Findings apply only to the target inspected.

"root" selects the session host running Skyhook and its configured workspace; "local" identifies that host's target type. Selecting another target uses its configured workspace; selecting the current remote target preserves your workspace override.

Remote commands receive Skyhook's SSH_AUTH_SOCK; Skyhook handles SSH authentication prompts."#;

const WORKSPACE_PROMPT: &str = "Workspaces set the base directory; they do not isolate files. Agents on the same target share its filesystem. Paths/cwd may be absolute or relative (including `..`) on the selected machine. Relative child workspace overrides resolve against the selected base.";
const LIFECYCLE_PROMPT: &str = "Direct tool calls return JobView; Result in tool descriptions refers to its result field. JavaScript calls return native results; background launches return job metadata. Use job_output to retrieve truncated results.\n\nCommand timeouts terminate execution; omitted timeouts have no deadline. Nonzero exit_code is a normal result. Script failures throw with partial error.output. Never circumvent a declined operation; use permitted alternatives or explain the limitation.";
const AGENT_PROMPT: &str = "Children start without your conversation history. Supply their task, relevant context, and scope, and give each child a distinct responsibility.\n\nLet children continue working autonomously. Use the appended runtime state and child messages to decide whether intervention is needed. Elapsed time, a wait timeout, or unchanged turn/tool-call counts alone do not establish that a child is stalled; it may be processing a request or awaiting a tool. When dependent on unfinished work, wait again. Send follow-ups to answer questions, resolve concrete blockers, correct a demonstrated misunderstanding, or communicate changed requirements. Resolve questions from children you supervise. Progress updates do not require a reply. When spawning a child, consider using todos to give it an initial checklist for multi-step work";

const STATE_PROMPT: &str = "The final <skyhook_state> is a fresh snapshot: date is local YYYY-MM-DD; absent jobs/todos sections are empty. Job rows follow the column header; parent is the containing agent job, or - for a top-level entry (not ownerless). Ages are seconds since creation; turns/tool_calls are exclusive per-agent counters. - means absent/not applicable, not zero. Strings use JSON quoting when needed; todo text and location overrides are always quoted. Omitted target/workspace equal skyhook_context, never the parent row. Todo status headings group consecutive items, preserving order including completed items.";

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

pub(super) fn system_segment(
    instructions: &[String],
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
    parts.push(STATE_PROMPT.to_owned());
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
        Capability::Read
        | Capability::Write
        | Capability::Exec
        | Capability::Network
        | Capability::Interactive
        | Capability::Mcp => None,
    }
}

pub(super) async fn runtime_state_content(
    jobs: &JobManager,
    todos: &TodoStore,
    agent: &AgentId,
    capabilities: &CapabilitySet,
    location: &ExecutionLocation,
) -> UserContent {
    let now = Local::now();
    let items = todos
        .inspect(agent, None)
        .await
        .expect("own todo list is always readable")
        .items;
    runtime_state_with_todos_at(jobs, agent, capabilities, items, location, now).await
}

/// Preview a candidate compaction's state without publishing its todos.
pub(super) async fn runtime_state_with_todos(
    jobs: &JobManager,
    agent: &AgentId,
    capabilities: &CapabilitySet,
    todos: Vec<TodoItem>,
    location: &ExecutionLocation,
) -> UserContent {
    runtime_state_with_todos_at(jobs, agent, capabilities, todos, location, Local::now()).await
}

async fn runtime_state_with_todos_at(
    jobs: &JobManager,
    agent: &AgentId,
    capabilities: &CapabilitySet,
    todos: Vec<TodoItem>,
    location: &ExecutionLocation,
    now: DateTime<Local>,
) -> UserContent {
    let active_jobs = jobs
        .active_states(agent, capabilities, now.timestamp_millis())
        .await;
    UserContent::Runtime {
        text: render_state(
            &now.format("%Y-%m-%d").to_string(),
            &active_jobs,
            &todos,
            location,
        ),
    }
}

/// This presentation is independent of the JSON used by tools and the journal.
fn render_state(
    date: &str,
    jobs: &[ActiveJob],
    todos: &[TodoItem],
    location: &ExecutionLocation,
) -> String {
    let mut text = format!("<skyhook_state>\ndate:{date}\n");
    if !jobs.is_empty() {
        text.push_str("jobs: job parent tool name state age_s turns tool_calls\n");
        render_jobs(&mut text, jobs, None, location);
    }
    if !todos.is_empty() {
        text.push_str("todos:\n");
        let mut previous = None;
        for item in todos {
            if previous != Some(item.status) {
                let status = match item.status {
                    TodoStatus::Pending => "pending",
                    TodoStatus::InProgress => "in_progress",
                    TodoStatus::Completed => "completed",
                };
                writeln!(text, "{status}:").unwrap();
                previous = Some(item.status);
            }
            writeln!(text, "  {}", quoted(&item.text)).unwrap();
        }
    }
    text.push_str("</skyhook_state>");
    text
}

fn render_jobs(
    text: &mut String,
    jobs: &[ActiveJob],
    parent: Option<JobId>,
    location: &ExecutionLocation,
) {
    for job in jobs {
        let state = serde_json::to_string(&job.state).expect("job state is serializable");
        write!(
            text,
            "{} {} {} {} {} {}",
            job.job,
            parent.map_or_else(|| "-".to_owned(), |id| id.to_string()),
            cell(&job.tool),
            job.name.as_deref().map_or_else(|| "-".to_owned(), cell),
            state.trim_matches('"'),
            job.age_seconds,
        )
        .unwrap();
        if let Some(progress) = job.progress {
            write!(text, " {} {}", progress.turns, progress.tool_calls).unwrap();
        } else {
            text.push_str(" - -");
        }
        // Every row compares against the snapshot context, never its parent row.
        if let Some(target) = &job.location.target
            && target != &location.target
        {
            write!(text, " target={}", quoted(target)).unwrap();
        }
        if job.location.workspace != location.workspace {
            write!(
                text,
                " workspace={}",
                quoted(&job.location.workspace.to_string_lossy())
            )
            .unwrap();
        }
        text.push('\n');
        render_jobs(text, &job.children, Some(job.job), location);
    }
}

fn quoted(value: &str) -> String {
    serde_json::to_string(value).expect("strings are serializable")
}

/// Keep common tool/job names bare, but escape anything that could alter a row.
fn cell(value: &str) -> String {
    if !value.is_empty()
        && value != "-"
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-./".contains(&byte))
    {
        value.to_owned()
    } else {
        quoted(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{ActiveJobLocation, JobState};

    fn location() -> ExecutionLocation {
        ExecutionLocation::root("/project".into())
    }

    fn job(id: u64, tool: &str, workspace: &str) -> ActiveJob {
        ActiveJob {
            job: JobId::new(id).unwrap(),
            tool: tool.to_owned(),
            name: None,
            state: JobState::Running,
            location: ActiveJobLocation {
                target: Some("root".to_owned()),
                workspace: workspace.into(),
            },
            age_seconds: 12,
            progress: (tool == "agent").then(Default::default),
            children: Vec::new(),
        }
    }

    #[test]
    fn empty_state_contains_only_date() {
        let rendered = render_state("2026-09-10", &[], &[], &location());
        assert_eq!(
            rendered,
            "<skyhook_state>\ndate:2026-09-10\n</skyhook_state>"
        );
    }

    #[test]
    fn todos_group_only_consecutive_statuses_and_escape_text() {
        let todos = [
            (TodoStatus::InProgress, "Design"),
            (TodoStatus::InProgress, "Measure"),
            (TodoStatus::Pending, "Implement"),
            (TodoStatus::Pending, "Test\n\"quoted\" \\ Unicode: λ"),
            (TodoStatus::Completed, "Inspect"),
            (TodoStatus::Pending, "Document"),
        ]
        .map(|(status, text)| TodoItem {
            status,
            text: text.to_owned(),
        });
        assert_eq!(
            render_state("2026-09-10", &[], &todos, &location()),
            concat!(
                "<skyhook_state>\ndate:2026-09-10\ntodos:\n",
                "in_progress:\n  \"Design\"\n  \"Measure\"\n",
                "pending:\n  \"Implement\"\n  \"Test\\n\\\"quoted\\\" \\\\ Unicode: λ\"\n",
                "completed:\n  \"Inspect\"\npending:\n  \"Document\"\n",
                "</skyhook_state>"
            )
        );
    }

    #[test]
    fn job_rows_keep_snapshot_locations_exact_counters_and_row_structure() {
        // Nested jobs use their own snapshot location, not the parent's.
        let mut parent = job(7, "agent", "/other");
        parent.name = Some("runtime-review".to_owned());
        parent.location.target = Some("remote".to_owned());
        parent.progress.as_mut().unwrap().turns = 3;
        parent.progress.as_mut().unwrap().tool_calls = 8;
        let mut child = job(9, "agent", "/project");
        child.state = JobState::WaitingInput;
        let mut grandchild = job(12, "agent", "/other");
        grandchild.location.target = Some("remote".to_owned());
        child.children.push(grandchild);
        parent.children.push(child);
        // Target and workspace overrides are independent; hidden targets stay hidden.
        let mut target_only = job(1, "agent", "/project");
        target_only.location.target = Some("remote".to_owned());
        let mut hidden_target = job(3, "exec", "/project");
        hidden_target.location.target = None;
        // A remote snapshot uses its own context as the default.
        let mut same = job(1, "agent", "/remote-project");
        same.location.target = Some("remote".to_owned());
        let remote = ExecutionLocation::named("remote", "/remote-project".into());
        // Arbitrary strings cannot change row structure.
        let mut item = job(1, "custom\n\"tool\"", "/other\n\"dir\"");
        item.name = Some("-".to_owned());
        item.location.target = Some("remote\nserver".to_owned());
        let cases = [
            (
                location(),
                vec![parent, job(18, "exec", "/project")],
                concat!(
                    "7 - agent runtime-review running 12 3 8 target=\"remote\" workspace=\"/other\"\n",
                    "9 7 agent - waiting_input 12 0 0\n",
                    "12 9 agent - running 12 0 0 target=\"remote\" workspace=\"/other\"\n",
                    "18 - exec - running 12 - -\n",
                ),
            ),
            (
                location(),
                vec![target_only, job(2, "exec", "/other"), hidden_target],
                concat!(
                    "1 - agent - running 12 0 0 target=\"remote\"\n",
                    "2 - exec - running 12 - - workspace=\"/other\"\n",
                    "3 - exec - running 12 - -\n",
                ),
            ),
            (
                remote,
                vec![same, job(2, "exec", "/project")],
                concat!(
                    "1 - agent - running 12 0 0\n",
                    "2 - exec - running 12 - - target=\"root\" workspace=\"/project\"\n",
                ),
            ),
            (
                location(),
                vec![item],
                "1 - \"custom\\n\\\"tool\\\"\" \"-\" running 12 - - target=\"remote\\nserver\" workspace=\"/other\\n\\\"dir\\\"\"\n",
            ),
        ];
        for (context, jobs, rows) in cases {
            let header = "jobs: job parent tool name state age_s turns tool_calls";
            let expected =
                format!("<skyhook_state>\ndate:2026-09-10\n{header}\n{rows}</skyhook_state>");
            assert_eq!(render_state("2026-09-10", &jobs, &[], &context), expected);
        }
        assert_eq!(cell("run-tests"), "run-tests");
        assert_eq!(cell("-"), "\"-\"");
        assert_eq!(cell(""), "\"\"");
        assert_eq!(cell("two words"), "\"two words\"");
        assert_eq!(cell("λ"), "\"λ\"");
    }
}

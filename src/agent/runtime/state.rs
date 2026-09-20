use std::fmt::Write;

use chrono::Local;

use crate::{
    agent::{TodoItem, TodoStatus, todo::TodoStore},
    execution::ExecutionLocation,
    identity::{AgentId, JobId},
    job::{ActiveJob, JobManager},
    provider::protocol::UserContent,
    tool::policy::CapabilitySet,
};

pub(super) async fn runtime_state_content(
    jobs: &JobManager,
    todos: &TodoStore,
    agent: &AgentId,
    capabilities: &CapabilitySet,
    location: &ExecutionLocation,
) -> UserContent {
    let items = todos
        .inspect(agent, None)
        .await
        .expect("own todo list is always readable")
        .items;
    runtime_state_with_todos(jobs, agent, capabilities, items, location).await
}

/// Preview a candidate compaction's state without publishing its todos.
pub(super) async fn runtime_state_with_todos(
    jobs: &JobManager,
    agent: &AgentId,
    capabilities: &CapabilitySet,
    todos: Vec<TodoItem>,
    location: &ExecutionLocation,
) -> UserContent {
    let now = Local::now();
    let active_jobs = jobs
        .active_states(agent, capabilities, now.timestamp_millis())
        .await;
    let date = now.format("%Y-%m-%d").to_string();
    let text = render_state(&date, &active_jobs, &todos, location);
    UserContent::Runtime { text }
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

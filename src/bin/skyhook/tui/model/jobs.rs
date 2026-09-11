//! Pending calls and admitted jobs rendered as structured tool cards.

use super::super::format::brief;
use super::super::tool_view::{Document, Role, Run, Section};
use super::{Entry, JobInfo, Projection, Surface, View};
use serde_json::Value;
use skyhook::identity::{AgentId, JobId};
use skyhook::job::JobState;
use skyhook::provider::protocol::ToolResult;
use std::collections::HashMap;

pub fn state_name(state: JobState) -> &'static str {
    match state {
        JobState::Queued => "Queued",
        JobState::AwaitingApproval => "Waiting for permission",
        JobState::Running => "Running",
        JobState::WaitingInput => "Waiting for input",
        JobState::Completed => "Completed",
        JobState::Failed => "Failed",
        JobState::Cancelled => "Cancelled",
        JobState::Interrupted => "Interrupted",
    }
}
pub(super) fn state_role(state: JobState) -> Role {
    match state {
        JobState::Running => Role::Indicator,
        JobState::AwaitingApproval | JobState::WaitingInput => Role::Warning,
        JobState::Completed => Role::Success,
        JobState::Failed => Role::Error,
        JobState::Queued | JobState::Cancelled | JobState::Interrupted => Role::Muted,
    }
}

pub(super) fn header_text(runs: &[Run]) -> String {
    runs.iter().map(Run::text).collect()
}

pub fn target_suffix(target: &str) -> String {
    if target == "root" {
        String::new()
    } else {
        format!(" @{target}")
    }
}

pub(super) fn call_entry(
    key: String,
    tool: &str,
    args: Option<&Value>,
    result: Option<&ToolResult>,
    agent: &AgentId,
    projection: &Projection,
    open: bool,
) -> Entry {
    let mut header = vec![
        Run::new(if open { "▾" } else { "▸" }, Role::Indicator),
        Run::new(" ", Role::Plain),
    ];
    if let Some(result) = result {
        header.push(Run::new(
            if result.is_error { "×" } else { "✓" },
            if result.is_error {
                Role::Error
            } else {
                Role::Success
            },
        ));
        header.push(Run::new(" ", Role::Plain));
    }
    header.push(Run::new(tool, Role::ToolName));
    if tool == "agent"
        && let Some(args) = args
    {
        header.push(Run::new(
            target_suffix(projection.child_target(agent, args)),
            Role::Target,
        ));
    }
    if let Some(result) = result {
        header.push(Run::new(" · ", Role::Muted));
        header.push(Run::new(
            if result.is_error {
                "Failed"
            } else {
                "Completed"
            },
            if result.is_error {
                Role::Error
            } else {
                Role::Success
            },
        ));
    }
    let mut entry = Entry::new(key, header_text(&header), Surface::Tool);
    entry.expandable = true;
    entry.header = Some(header.clone());
    if open {
        let mut document = Document::default();
        document.sections.push(Section::Line(header));
        if let Some(args) = args {
            document.arguments(tool, args);
        }
        if let Some(result) = result {
            document.output(tool, args.unwrap_or(&Value::Null), &result.result);
        }
        entry.text = document.plain_text();
        entry.document = Some(document);
    }
    entry
}

pub(super) fn job_entry(
    job: &JobInfo,
    projection: &Projection,
    view: &View,
    outputs: &HashMap<JobId, Value>,
    all: bool,
) -> Entry {
    let key = format!("j{}", job.id);
    let open = view.is_expanded(&key, all);
    let detail = match job.tool.as_str() {
        "exec" => job
            .args
            .get("argv")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default(),
        "shell" => job.args["command"].as_str().unwrap_or_default().into(),
        "script" => "JavaScript workflow".into(),
        _ => job
            .args
            .get("path")
            .or_else(|| job.args.get("pattern"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
    };
    let symbol = match job.state {
        JobState::Completed => "✓",
        JobState::Failed => "×",
        JobState::AwaitingApproval => "◇",
        JobState::WaitingInput => "?",
        JobState::Running => "●",
        _ => "·",
    };
    let header = vec![
        Run::new(if open { "▾" } else { "▸" }, Role::Indicator),
        Run::new(" ", Role::Plain),
        Run::new(symbol, state_role(job.state)),
        Run::new(" ", Role::Plain),
        Run::new(job.tool.clone(), Role::ToolName),
        Run::new(target_suffix(projection.job_target(job)), Role::Target),
        Run::new(format!(" {}", brief(&detail, 90)), Role::Plain),
        Run::new(" · ", Role::Muted),
        Run::new(state_name(job.state), state_role(job.state)),
        Run::new(" · ", Role::Muted),
        Run::new(format!("#{}", job.id), Role::Muted),
    ];
    let mut text = header_text(&header);
    let mut document = None;
    if open {
        let mut body = Document::default();
        body.sections.push(Section::Line(header.clone()));
        body.line(job.location.clone(), Role::Muted);
        body.arguments(&job.tool, &job.args);
        if outputs.contains_key(&job.id) || job.error.is_some() {
            body.output_with_error(
                &job.tool,
                &job.args,
                outputs.get(&job.id),
                job.error.as_deref(),
            );
        } else if job.remote && !job.state.is_terminal() {
            body.line(
                "Running remotely · output available after completion",
                Role::Muted,
            );
        } else {
            body.line("Loading output…", Role::Muted);
        }
        body.line(
            "[o] output fields / search / next page    [c] cancel job",
            Role::Muted,
        );
        text = body.plain_text();
        document = Some(body);
    }
    let mut entry = Entry::new(key, text, Surface::Tool);
    entry.document = document;
    entry.header = Some(header);
    entry.expandable = true;
    entry.job = Some(job.id);
    let mut parent = job.parent;
    while let Some(p) = parent.and_then(|id| projection.jobs.get(&id)) {
        entry.indent = entry.indent.saturating_add(2).min(16);
        parent = p.parent;
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use skyhook::identity::SessionId;

    #[test]
    fn expanded_jobs_and_historical_call_results_omit_null_object_fields() {
        let agent = AgentId::root(SessionId::from_bytes([1; 16]));
        let id = JobId::new(42).unwrap();
        let result = ToolResult {
            call_id: "old-call".into(),
            name: "exec".into(),
            result: serde_json::json!({
                "error": null, "result": {
                    "absent": null, "items": [null, {"absent": null, "keep": false}],
                    "stdout": "  literal null\t\n"
                }
            }),
            images: vec![],
            is_error: false,
        };
        let before = result.result.clone();
        let job = JobInfo {
            id,
            agent: agent.clone(),
            name: None,
            tool: "exec".into(),
            args: serde_json::json!({}),
            parent: None,
            state: JobState::Completed,
            target: "root".into(),
            location: "/workspace".into(),
            remote: false,
            error: None,
        };
        let projection = Projection::default();
        let outputs = HashMap::from([(id, result.result.clone())]);
        let entries = [
            call_entry(
                "old-call".into(),
                "exec",
                None,
                Some(&result),
                &agent,
                &projection,
                true,
            ),
            job_entry(&job, &projection, &View::default(), &outputs, true),
        ];
        for entry in entries {
            assert!(!entry.text.contains("absent"));
            assert!(!entry.text.contains("\"error\""));
            let document = entry.document.unwrap();
            assert!(document.sections.iter().any(|section| {
                matches!(section, Section::Code { source, .. } if &**source == "  literal null\t\n")
            }));
            assert!(document.sections.iter().any(|section| {
                matches!(section, Section::Code { source, .. }
                    if serde_json::from_str::<Value>(source).ok()
                        == Some(serde_json::json!({"result": {"items": [null, {"keep": false}]}})))
            }));
        }
        assert_eq!(result.result, before);
        assert_eq!(outputs[&id], before);
    }

    #[test]
    fn job_headers_preserve_historical_state_semantics() {
        let projection = Projection::default();
        let states = [
            (JobState::Queued, "·", "Queued", Role::Muted),
            (
                JobState::AwaitingApproval,
                "◇",
                "Waiting for permission",
                Role::Warning,
            ),
            (JobState::Running, "●", "Running", Role::Indicator),
            (
                JobState::WaitingInput,
                "?",
                "Waiting for input",
                Role::Warning,
            ),
            (JobState::Completed, "✓", "Completed", Role::Success),
            (JobState::Failed, "×", "Failed", Role::Error),
            (JobState::Cancelled, "·", "Cancelled", Role::Muted),
            (JobState::Interrupted, "·", "Interrupted", Role::Muted),
        ];
        for (state, symbol, name, role) in states {
            for target in ["root", "build-host"] {
                let job = JobInfo {
                    id: JobId::new(42).unwrap(),
                    agent: AgentId::root(SessionId::from_bytes([1; 16])),
                    name: None,
                    tool: "exec".into(),
                    args: serde_json::json!({"argv": ["echo", "Failed @fake Completed"]}),
                    parent: None,
                    state,
                    target: target.into(),
                    location: "/workspace".into(),
                    remote: false,
                    error: Some("Failure details\nsecond line".into()),
                };
                let collapsed =
                    job_entry(&job, &projection, &View::default(), &HashMap::new(), false);
                let expanded =
                    job_entry(&job, &projection, &View::default(), &HashMap::new(), true);
                for (entry, arrow) in [(&collapsed, "▸"), (&expanded, "▾")] {
                    let expected = format!(
                        "{arrow} {symbol} exec{} echo Failed @fake Completed · {name} · #42",
                        target_suffix(target)
                    );
                    let runs = entry.header.as_ref().unwrap();
                    assert_eq!(header_text(runs), expected);
                    assert_eq!(entry.text.lines().next().unwrap(), expected);
                    assert_eq!(runs[2], Run::new(symbol, role));
                    assert_eq!(runs[8], Run::new(name, role));
                }

                assert_eq!(
                    collapsed.text,
                    header_text(collapsed.header.as_ref().unwrap())
                );
                assert!(collapsed.document.is_none());
                let document = expanded.document.as_ref().unwrap();
                assert_eq!(
                    document.sections[0],
                    Section::Line(expanded.header.clone().unwrap())
                );
                assert_eq!(
                    collapsed.header.as_ref().unwrap()[1..],
                    expanded.header.as_ref().unwrap()[1..]
                );
            }
        }
    }

    #[test]
    fn failed_jobs_put_errors_in_expanded_output_and_keep_real_results() {
        let id = JobId::new(42).unwrap();
        let job = JobInfo {
            id,
            agent: AgentId::root(SessionId::from_bytes([74; 16])),
            name: None,
            tool: "exec".into(),
            args: serde_json::json!({"argv": ["echo"]}),
            parent: None,
            state: JobState::Failed,
            target: "root".into(),
            location: "/workspace".into(),
            remote: false,
            error: Some("failed exactly".into()),
        };
        let projection = Projection::default();
        for output in [
            None,
            Some(
                serde_json::json!({"error": "failed exactly", "result": {"stdout": "  saved output\t\n"}}),
            ),
            Some(serde_json::json!({"result": {"stderr": "other details"}})),
        ] {
            let outputs: HashMap<_, _> = output.into_iter().map(|output| (id, output)).collect();
            let collapsed = job_entry(&job, &projection, &View::default(), &outputs, false);
            assert_eq!(collapsed.text.lines().count(), 1);
            assert!(!collapsed.text.contains("failed exactly"));
            assert!(collapsed.document.is_none());
            let expanded = job_entry(&job, &projection, &View::default(), &outputs, true);
            assert!(expanded.text.contains("Output\n  failed exactly"));
            assert_eq!(expanded.text.matches("failed exactly").count(), 1);
            assert!(!expanded.text.contains("Loading output"));
            if let Some(stdout) = outputs
                .get(&id)
                .and_then(|value| value.pointer("/result/stdout"))
            {
                assert!(expanded.document.as_ref().unwrap().sections.iter().any(|section| {
                    matches!(section, Section::Code { source, .. } if &**source == stdout.as_str().unwrap())
                }));
            }
            if outputs
                .get(&id)
                .is_some_and(|value| value.pointer("/result/stderr").is_some())
            {
                assert!(expanded.text.contains("other details"));
            }
        }
    }

    #[test]
    fn pending_agent_calls_share_their_target_and_header_with_the_document() {
        let agent = AgentId::root(SessionId::from_bytes([3; 16]));
        let projection = Projection::default();
        let args = serde_json::json!({"target": "build-host"});
        for open in [false, true] {
            let entry = call_entry(
                "call".into(),
                "agent",
                Some(&args),
                None,
                &agent,
                &projection,
                open,
            );
            let header = entry.header.as_ref().unwrap();
            assert_eq!(
                header_text(header),
                format!("{} agent @build-host", if open { "▾" } else { "▸" })
            );
            assert_eq!(header.last(), Some(&Run::new(" @build-host", Role::Target)));
            if open {
                assert_eq!(
                    entry.document.unwrap().sections[0],
                    Section::Line(header.clone())
                );
            } else {
                assert!(entry.document.is_none());
            }
        }
    }
}

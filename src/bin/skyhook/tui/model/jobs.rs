//! Pending calls and admitted jobs rendered as structured tool cards.

use crate::tui::app::OutputStore;

use super::super::format::brief;
#[cfg(test)]
use super::super::tool_view::Section;
use super::super::tool_view::{Document, Role, Run};
use super::{Entry, EntryKey, JobInfo, Projection, View};
use serde_json::Value;
use skyhook::identity::AgentId;
#[cfg(test)]
use skyhook::job::JobRole;
use skyhook::job::JobState;
use skyhook::provider::protocol::ToolResult;

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

#[cfg(test)]
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
    key: EntryKey,
    (tool, args, result): (
        &str,
        Option<&serde_json::Map<String, Value>>,
        Option<&ToolResult>,
    ),
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
    // Unadmitted calls have only provider names/arguments, not a JobCreated
    // role; admitted calls use job_entry instead. Decode the known agent
    // argument schema solely for this target label, never lifecycle/ownership.
    if tool == "agent"
        && let Some(args) = args
    {
        let target = args
            .get("target")
            .and_then(Value::as_str)
            .unwrap_or_else(|| projection.child_target(agent, &Value::Null));
        header.push(Run::new(target_suffix(target), Role::Target));
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
    let document = if open {
        // Do not allocate a Value for collapsed cards; only the existing
        // Value-based structured formatters require this adapter.
        let args_value = args.map(|args| Value::Object(args.clone()));
        let args = args_value.as_ref();
        let mut document = Document::default();
        if let Some(args) = args {
            document.arguments(tool, args);
        }
        if let Some(result) = result {
            document.output(tool, args.unwrap_or(&Value::Null), &result.result);
        }
        Some(document)
    } else {
        None
    };
    Entry::card(key, header, document)
}

pub(super) fn job_entry(
    job: &JobInfo,
    projection: &Projection,
    view: &View,
    outputs: &OutputStore,
    all: bool,
) -> Entry {
    let key = EntryKey::Job(job.id);
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
    let mut document = None;
    if open {
        let mut body = Document::default();
        body.line(job.location_label(), Role::Muted);
        body.arguments(&job.tool, &job.args);
        if outputs.get(&job.id).is_some() || job.error.is_some() {
            body.output_with_error(
                &job.tool,
                &job.args,
                outputs.get(&job.id),
                job.error.as_deref(),
            );
        } else if job.remote() && !job.state.is_terminal() {
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
        document = Some(body);
    }
    let mut entry = Entry::card(key, header, document);
    let mut parent = job.parent;
    while let Some(p) = parent.and_then(|id| projection.jobs.get(&id)) {
        entry.indent = entry.indent.saturating_add(2).min(16);
        parent = p.parent;
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::super::tests::{job_info, root};
    use super::*;
    use crate::tui::app::OutputStore;
    use crate::tui::tool_view::OutputView;
    use skyhook::execution::ExecutionLocation;
    use skyhook::provider::protocol::ToolCall;

    fn has_code(entry: &Entry, test: impl Fn(&str) -> bool) -> bool {
        let sections = &entry.document().unwrap().sections;
        sections
            .iter()
            .any(|section| matches!(section, Section::Code { source, .. } if test(source)))
    }

    #[test]
    fn expanded_jobs_and_historical_call_results_omit_null_object_fields() {
        let agent = root(1);
        let value = serde_json::json!({
            "error": null, "result": {
                "absent": null, "items": [null, {"absent": null, "keep": false}],
                "stdout": "  literal null\t\n"
            }
        });
        let result = ToolResult {
            call_id: "old-call".into(),
            name: "exec".into(),
            result: value.clone(),
            images: vec![],
            is_error: false,
        };
        let job = job_info(&agent, 42, JobRole::Tool, JobState::Completed);
        let (projection, mut outputs) = (Projection::default(), OutputStore::default());
        outputs.insert_product(job.id, OutputView::historical(value.clone()));
        let call = ("exec", None, Some(&result));
        let kept = serde_json::json!({"result": {"items": [null, {"keep": false}]}});
        for entry in [
            call_entry(EntryKey::Record(1), call, &agent, &projection, true),
            job_entry(&job, &projection, &View::default(), &outputs, true),
        ] {
            assert!(!entry.text().contains("absent") && !entry.text().contains("\"error\""));
            assert!(has_code(&entry, |source| source == "  literal null\t\n"));
            assert!(has_code(&entry, |source| {
                serde_json::from_str::<Value>(source).ok().as_ref() == Some(&kept)
            }));
        }
        assert_eq!(result.result, value);
        assert_eq!(outputs.get(&job.id).unwrap().value(), &value);
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
                    args: serde_json::json!({"argv": ["echo", "Failed @fake Completed"]}),
                    location: ExecutionLocation::named(target, "/workspace".into()),
                    error: Some("Failure details\nsecond line".into()),
                    ..job_info(&root(1), 42, JobRole::Tool, state)
                };
                let [collapsed, expanded] = [false, true].map(|open| {
                    job_entry(
                        &job,
                        &projection,
                        &View::default(),
                        &OutputStore::default(),
                        open,
                    )
                });
                for (entry, arrow) in [(&collapsed, "▸"), (&expanded, "▾")] {
                    let expected = format!(
                        "{arrow} {symbol} exec{} echo Failed @fake Completed · {name} · #42",
                        target_suffix(target)
                    );
                    let runs = entry.header().unwrap();
                    assert_eq!(header_text(runs), expected);
                    assert_eq!(entry.text().lines().next().unwrap(), expected);
                    assert_eq!(
                        (&runs[2], &runs[8]),
                        (&Run::new(symbol, role), &Run::new(name, role))
                    );
                }
                assert_eq!(collapsed.text(), header_text(collapsed.header().unwrap()));
                assert!(collapsed.document().is_none());
                assert_eq!(
                    collapsed.header().unwrap()[1..],
                    expanded.header().unwrap()[1..]
                );
            }
        }
    }

    #[test]
    fn failed_jobs_put_errors_in_expanded_output_and_keep_real_results() {
        let job = JobInfo {
            error: Some("failed exactly".into()),
            ..job_info(&root(74), 42, JobRole::Tool, JobState::Failed)
        };
        let projection = Projection::default();
        for output in [
            None,
            Some(
                serde_json::json!({"error": "failed exactly", "result": {"stdout": "  saved output\t\n"}}),
            ),
            Some(serde_json::json!({"result": {"stderr": "other details"}})),
        ] {
            let mut outputs = OutputStore::default();
            if let Some(output) = output.clone() {
                outputs.insert_product(job.id, OutputView::historical(output));
            }
            let collapsed = job_entry(&job, &projection, &View::default(), &outputs, false);
            assert_eq!(collapsed.text().lines().count(), 1);
            assert!(!collapsed.text().contains("failed exactly"));
            assert!(collapsed.document().is_none());
            let expanded = job_entry(&job, &projection, &View::default(), &outputs, true);
            assert!(expanded.text().contains("Output\n  failed exactly"));
            assert_eq!(expanded.text().matches("failed exactly").count(), 1);
            assert!(!expanded.text().contains("Loading output"));
            let result = output.as_ref().map(|output| &output["result"]);
            if let Some(Value::String(stdout)) = result.map(|result| &result["stdout"]) {
                assert!(has_code(&expanded, |source| source == stdout));
            }
            if result.is_some_and(|result| result.get("stderr").is_some()) {
                assert!(expanded.text().contains("other details"));
            }
        }
    }

    #[test]
    fn pending_agent_calls_share_their_target_and_header_with_the_document() {
        let agent = root(3);
        let projection = Projection::default();
        let call =
            ToolCall::new("call", "agent", serde_json::json!({"target": "build-host"})).unwrap();
        for open in [false, true] {
            let fields = (call.name(), Some(call.arguments()), None);
            let entry = call_entry(EntryKey::Record(1), fields, &agent, &projection, open);
            let header = entry.header().unwrap();
            let arrow = if open { "▾" } else { "▸" };
            assert_eq!(header_text(header), format!("{arrow} agent @build-host"));
            assert_eq!(header.last(), Some(&Run::new(" @build-host", Role::Target)));
            assert_eq!(entry.document().is_some(), open);
        }
    }
}

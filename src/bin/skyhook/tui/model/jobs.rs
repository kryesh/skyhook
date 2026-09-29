//! Pending calls and admitted jobs rendered as structured tool cards.

use super::super::tool_view::{Document, Hints, Role, Run, starts_child};
use super::{Entry, EntryKey, EntryView, JobInfo, Projection};
use crate::text::brief;
use crate::tui::app::OutputStore;
use crate::tui::format::{Precision, local_time};
use chrono::Local;
use serde_json::Value;
use skyhook::identity::AgentId;
use skyhook::job::JobState;
use skyhook::provider::protocol::ToolResult;
use skyhook::target::TargetRef;

/// Characters of a card header's free-text detail, such as a command or name.
pub(super) const HEADER_DETAIL: usize = 80;

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
pub fn state_glyph(state: JobState) -> &'static str {
    match state {
        JobState::Queued => "·",
        JobState::AwaitingApproval => "◇",
        JobState::Running => "●",
        JobState::WaitingInput => "?",
        JobState::Completed => "✓",
        JobState::Failed => "×",
        JobState::Cancelled | JobState::Interrupted => "■",
    }
}
pub fn state_role(state: JobState) -> Role {
    match state {
        JobState::Running => Role::Indicator,
        JobState::AwaitingApproval | JobState::WaitingInput => Role::Warning,
        JobState::Completed => Role::Success,
        JobState::Failed => Role::Error,
        JobState::Queued | JobState::Cancelled | JobState::Interrupted => Role::Muted,
    }
}

pub fn target_suffix(target: &TargetRef) -> String {
    target
        .name()
        .map_or_else(String::new, |name| format!(" @{name}"))
}

/// A child's target suffix; a request naming no valid target shows as given.
fn requested_suffix(target: Result<TargetRef, &str>) -> String {
    target.map_or_else(
        |requested| format!(" @{requested}"),
        |target| target_suffix(&target),
    )
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
    let state = result.map(|result| {
        if result.is_error {
            JobState::Failed
        } else {
            JobState::Completed
        }
    });
    let mut header = Vec::new();
    if let Some(state) = state {
        header.push(Run::new(state_glyph(state), state_role(state)));
        header.push(Run::new(" ", Role::Plain));
    }
    header.push(Run::new(tool, Role::ToolName));
    // Unadmitted calls have only provider names/arguments, not a JobCreated
    // role; admitted calls use job_entry instead.
    if starts_child(tool)
        && let Some(args) = args
    {
        let target = projection.child_target(agent, args.get(skyhook::tool::TARGET));
        header.push(Run::new(requested_suffix(target), Role::Target));
    }
    if let Some(state) = state {
        header.push(Run::new(" · ", Role::Muted));
        header.push(Run::new(state_name(state), state_role(state)));
    }
    let document = open.then(|| {
        // Collapsed cards never allocate a Value copy of the arguments.
        let args = args.map_or(Value::Null, |args| Value::Object(args.clone()));
        let hints = Hints::new(tool, &args);
        let mut document = Document::default();
        if !args.is_null() {
            document.arguments(hints);
        }
        if let Some(result) = result {
            document.historical_output(hints, &result.result);
        }
        document
    });
    Entry::card(key, header, document)
}

pub(super) fn job_entry(
    job: &JobInfo,
    projection: &Projection,
    outputs: &OutputStore,
    presentation: EntryView<'_>,
) -> Entry {
    let EntryView {
        view,
        all_details,
        today,
        ..
    } = presentation;
    let key = EntryKey::Job(job.id);
    let hints = Hints::new(&job.tool, &job.args);
    let mut header = vec![
        Run::new(state_glyph(job.state), state_role(job.state)),
        Run::new(" ", Role::Plain),
        Run::new(job.tool.clone(), Role::ToolName),
        Run::new(requested_suffix(projection.job_target(job)), Role::Target),
        Run::new(
            format!(" {}", brief(&hints.summary(), HEADER_DETAIL)),
            Role::Plain,
        ),
        Run::new(" · ", Role::Muted),
        Run::new(state_name(job.state), state_role(job.state)),
    ];
    if let Some(took) = job.timing.took() {
        header.push(Run::new(format!(" · {took}"), Role::Muted));
    }
    header.push(Run::new(format!(" · #{}", job.id), Role::Muted));
    let document = view.is_expanded(&key, all_details).then(|| {
        let mut body = Document::default();
        let mut location = job.location_label();
        if let Some(since) = job.timing.since() {
            let started = local_time(since, today, &Local, Precision::Seconds);
            location.push_str(&format!(" · started {started}"));
        }
        body.line(location, Role::Muted);
        body.arguments(hints);
        let output = outputs.get(&job.id);
        if output.is_some() || job.error.is_some() {
            let failure = output.and_then(|output| output.as_ref().err());
            let summaries: Vec<&str> = job
                .error
                .iter()
                .chain(failure)
                .map(String::as_str)
                .collect();
            let view = output.and_then(|output| output.as_ref().ok());
            body.output(hints, view, &summaries);
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
        body
    });
    let mut entry = Entry::card(key, header, document);
    entry.timing = job.timing;
    entry
}

#[cfg(test)]
mod tests {
    use super::super::super::tool_view::Section;
    use super::super::clean;
    use super::super::tests::{header_text, job_info, loaded, root};
    use super::super::{Tab, View};
    use super::*;
    use crate::tui::format::Timing;
    use skyhook::job::JobRole;
    use skyhook::provider::protocol::ToolCall;

    /// `job`'s card with every detail `open` or none.
    fn card(job: &JobInfo, outputs: &OutputStore, open: bool) -> Entry {
        let (agent, view) = (root(1), View::default());
        let presentation = EntryView {
            agent: &agent,
            tab: Tab::Conversation,
            view: &view,
            all_details: open,
            today: chrono::NaiveDate::default(),
        };
        job_entry(job, &Projection::default(), outputs, presentation)
    }

    #[test]
    fn job_cards_time_their_run_and_show_when_it_started_once_expanded() {
        let outputs = OutputStore::default();
        let started = local_time(1_000, Default::default(), &Local, Precision::Seconds);
        // A running job's counter is painted, never part of its header.
        for (state, timing, time) in [
            (JobState::Queued, Timing::Untimed, ""),
            (JobState::Running, Timing::Since(1_000), ""),
            (
                JobState::Completed,
                Timing::Took {
                    since: 1_000,
                    until: 4_200,
                },
                " · 3.2s",
            ),
        ] {
            let job = JobInfo {
                timing,
                ..job_info(&root(5), 42, JobRole::Tool, state)
            };
            let entry = |open| card(&job, &outputs, open);
            let header = header_text(entry(false).header().unwrap());
            assert!(header.ends_with(&format!("{}{time} · #42", state_name(state))));
            let expanded = entry(true).text().contains(&format!("started {started}"));
            assert_eq!(expanded, timing != Timing::Untimed, "{state:?}");
            assert_eq!(entry(false).timing, timing);
        }
    }

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
            "state": "completed", "result": {
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
        loaded(&mut outputs, job.id, value.clone());
        let call = ("exec", None, Some(&result));
        let kept = serde_json::json!({"items": [null, {"keep": false}]});
        for entry in [
            call_entry(EntryKey::UnsavedStatus(0), call, &agent, &projection, true),
            card(&job, &outputs, true),
        ] {
            assert!(!entry.text().contains("absent") && !entry.text().contains("\"error\""));
            assert!(has_code(&entry, |source| source == "  literal null    \n"));
            assert!(has_code(&entry, |source| {
                serde_json::from_str::<Value>(source).ok().as_ref() == Some(&kept)
            }));
        }
        assert_eq!(result.result, value);
    }

    #[test]
    fn failed_jobs_put_errors_in_expanded_output_and_keep_real_results() {
        let job = JobInfo {
            error: Some("failed exactly".into()),
            ..job_info(&root(74), 42, JobRole::Tool, JobState::Failed)
        };
        for output in [
            None,
            Some(serde_json::json!({"error": "failed exactly",
                "result": {"stdout": "  saved output\t\n"}})),
            Some(serde_json::json!({"result": {"stderr": "other details"}})),
        ] {
            let mut outputs = OutputStore::default();
            if let Some(output) = output.clone() {
                loaded(&mut outputs, job.id, output);
            }
            let collapsed = card(&job, &outputs, false);
            assert_eq!(collapsed.text().lines().count(), 1);
            assert!(!collapsed.text().contains("failed exactly"));
            assert!(collapsed.document().is_none());
            let expanded = card(&job, &outputs, true);
            assert!(expanded.text().contains("Output\n  failed exactly"));
            assert_eq!(expanded.text().matches("failed exactly").count(), 1);
            assert!(!expanded.text().contains("Loading output"));
            let result = output.as_ref().map(|output| &output["result"]);
            if let Some(Value::String(stdout)) = result.map(|result| &result["stdout"]) {
                assert!(has_code(&expanded, |source| source == clean(stdout)));
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
        // A target that fails admission still shows as the call requested it.
        for (target, open) in [
            ("build-host", false),
            ("build-host", true),
            ("bad name", true),
        ] {
            let args = serde_json::json!({"target": target});
            let call = ToolCall::new("call", "agent", args).unwrap();
            let fields = (call.name(), Some(call.arguments()), None);
            let entry = call_entry(
                EntryKey::UnsavedStatus(0),
                fields,
                &agent,
                &projection,
                open,
            );
            let header = entry.header().unwrap();
            let arrow = if open { "▾" } else { "▸" };
            assert_eq!(header_text(header), format!("{arrow} agent @{target}"));
            let suffix = Run::new(format!(" @{target}"), Role::Target);
            assert_eq!(header.last(), Some(&suffix));
            assert_eq!(entry.document().is_some(), open);
        }
    }
}

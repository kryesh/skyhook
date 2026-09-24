//! Historical job notifications, independent of the latest job state.

use super::super::format::brief;
use super::super::tool_view::{Document, Role, Run};
use super::jobs::state_role;
use super::{Entry, EntryKey, Projection, View, clean};
use serde_json::Value;
use skyhook::session::{JobEvent, RecordSeq};

/// Show the events where the model received them, using their historical payload
/// rather than the job's latest output (the same job may have since resumed).
pub(super) fn job_event_entries(
    record: RecordSeq,
    block: usize,
    events: &[JobEvent],
    projection: &Projection,
    view: &View,
    all: bool,
) -> Vec<Entry> {
    events
        .iter()
        .enumerate()
        .map(|(index, event)| {
            let key = EntryKey::Notification {
                record,
                block,
                event: index,
            };
            let open = view.is_expanded(&key, all);
            let mut header = vec![
                Run::new(if open { "▾" } else { "▸" }, Role::Indicator),
                Run::new(" Job event", Role::Plain),
                Run::new(" · ", Role::Muted),
            ];
            // A notification key is never a job key: output refresh must not
            // replace historical content with a live job card.
            let mut body = open.then(Document::default);
            match event {
                JobEvent::Message(message) => {
                    let job = projection.jobs.get(&message.id);
                    let tool = job.map_or("agent", |job| job.tool.as_str());
                    header.push(Run::new(tool, Role::ToolName));
                    header.push(Run::new(format!(" #{}", message.id), Role::Muted));
                    let name = message
                        .name
                        .as_deref()
                        .or_else(|| job.and_then(|job| job.name.as_deref()));
                    if let Some(name) = name {
                        header.push(Run::new(" · ", Role::Muted));
                        header.push(Run::new(brief(&clean(name), 80), Role::Plain));
                    }
                    header.push(Run::new(" · ", Role::Muted));
                    header.push(Run::new(
                        format!("message #{}", message.message),
                        Role::Plain,
                    ));
                    if let Some(body) = &mut body {
                        body.line("Agent message received by model", Role::Muted);
                        body.line(clean(&message.text), Role::Plain);
                    }
                }
                JobEvent::Job(job_view) => {
                    let id = job_view.id();
                    let job = id.and_then(|id| projection.jobs.get(&id));
                    let tool = job_view
                        .tool()
                        .or_else(|| job.map(|job| job.tool.as_str()))
                        .unwrap_or("job");
                    let state = job_view.state();
                    header.push(Run::new(tool, Role::ToolName));
                    header.push(Run::new(
                        id.map_or(String::new(), |id| format!(" #{id}")),
                        Role::Muted,
                    ));
                    header.push(Run::new(" · ", Role::Muted));
                    header.push(Run::new(state.to_string(), state_role(state)));
                    if let Some(body) = &mut body {
                        // The presented JSON is the event's contract; the card
                        // renders it as received.
                        let event = serde_json::to_value(job_view).expect("job views serialize");
                        body.line("Notification received by model", Role::Muted);
                        body.output(tool, job.map_or(&Value::Null, |job| &job.args), &event);
                    }
                }
            }
            Entry::card(key, header, body)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::JobInfo;
    use super::super::tests::{header_text, job_info, root};
    use super::*;
    use crate::tui::model::Surface;
    use skyhook::job::JobState;
    use skyhook::session::RecordSeq;

    fn events(events: Vec<Value>, projection: &Projection, view: &View, all: bool) -> Vec<Entry> {
        let events: Vec<JobEvent> = events
            .into_iter()
            .map(|event| serde_json::from_value(event).unwrap())
            .collect();
        job_event_entries(RecordSeq::default(), 0, &events, projection, view, all)
    }

    fn job(id: u64, state: &str) -> Value {
        serde_json::json!({
            "kind":"job", "id":id, "state":state, "has_result":false, "result":null, "error":null,
            "meta":{"parent":null,"tool":"exec","name":null,"target":null,"workspace":null,
                    "last_message":null,"code":null,"executed":null},
            "presentation":null
        })
    }

    #[test]
    fn notification_headers_use_only_historical_typed_states() {
        let cards = events(
            vec![
                job(1, "failed"),
                job(2, "completed"),
                serde_json::json!({"kind":"message", "id":3, "name":"Failed", "message":4, "text":"Completed"}),
            ],
            &Projection::default(),
            &View::default(),
            true,
        );
        let expected = [
            ("▾ Job event · exec #1 · failed", Role::Error),
            ("▾ Job event · exec #2 · completed", Role::Success),
            ("▾ Job event · agent #3 · Failed · message #4", Role::Plain),
        ];
        assert_eq!(cards.len(), expected.len());
        for (card, (text, role)) in cards.iter().zip(expected) {
            let runs = card.header().unwrap();
            assert_eq!(header_text(runs), text);
            let state = text.rsplit(" · ").next().unwrap();
            assert_eq!(runs.last().unwrap(), &Run::new(state, role));
        }
    }

    #[test]
    fn intermediate_agent_messages_are_historical_expandable_job_events() {
        let message = "Both reviewer gaps are fixed.\nChecking \"native\" replay — next.";
        let reply = vec![serde_json::json!({
            "kind":"message", "id":253, "name":"implement-native-replay", "message":6577, "text":message,
        })];
        let mut projection = Projection::default();
        let mut view = View::default();
        let collapsed = events(reply.clone(), &projection, &view, false);
        assert_eq!(collapsed.len(), 1);
        let card = &collapsed[0];
        assert_eq!(
            (card.surface, card.expandable(), card.job_id()),
            (Surface::Tool, true, None)
        );
        let key = EntryKey::Notification {
            record: RecordSeq::default(),
            block: 0,
            event: 0,
        };
        assert_eq!(card.key(), &key);
        let header = "Job event · agent #253 · implement-native-replay · message #6577";
        assert!(card.text().contains(header));
        assert!(!card.text().contains("skyhook_job_events") && !card.text().contains(message));
        view.set_expanded(key.clone(), true);
        let expanded = events(reply.clone(), &projection, &view, false);
        assert!(expanded[0].document().is_some());
        let expanded = expanded[0].text();
        assert!(expanded.contains(message));
        assert!(!expanded.contains("<skyhook_") && !expanded.contains("\\\"text\\\""));

        // A later terminal job snapshot must not relabel or replace the historical message.
        let job = JobInfo {
            name: Some("current name".into()),
            tool: "agent".into(),
            location: skyhook::execution::ExecutionLocation::named(
                "host".parse().unwrap(),
                ".".into(),
            ),
            ..job_info(
                &root(1),
                253,
                skyhook::job::JobRole::Agent,
                JobState::Completed,
            )
        };
        projection.jobs.insert(job.id, job);
        let after_completion = events(reply.clone(), &projection, &view, false);
        assert_eq!(after_completion[0].text(), expanded);
        assert!(after_completion[0].job_id().is_none());
        view.set_expanded(key, false);
        assert!(
            events(reply, &projection, &view, true)[0]
                .document()
                .is_none()
        );
    }
}

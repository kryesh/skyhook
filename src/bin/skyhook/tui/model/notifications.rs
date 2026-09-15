//! Historical job notifications, independent of the latest job state.

use super::super::format::brief;
use super::super::tool_view::{Document, Role, Run};
#[cfg(test)]
use super::jobs::header_text;
use super::jobs::state_role;
use super::{Entry, EntryKey, Projection, Surface, View, clean};
use serde_json::Value;
use skyhook::identity::JobId;
use skyhook::job::JobState;

/// Keep recognizing the original envelopes when projecting saved sessions.
#[derive(Clone, Copy)]
pub(super) enum JobNotificationKind {
    State,
    AgentMessage,
}

impl JobNotificationKind {
    fn tags(self) -> (&'static str, &'static str) {
        match self {
            Self::State => ("<skyhook_job_events>", "</skyhook_job_events>"),
            Self::AgentMessage => ("<skyhook_agent_messages>", "</skyhook_agent_messages>"),
        }
    }
}

pub(super) fn job_notification_kind(text: &str) -> Option<JobNotificationKind> {
    [
        JobNotificationKind::State,
        JobNotificationKind::AgentMessage,
    ]
    .into_iter()
    .find(|kind| text.trim_start().starts_with(kind.tags().0))
}

/// Show the event where the model received it, using its historical payload
/// rather than the job's latest output (the same job may have since resumed).
/// Both runtime envelopes describe historical job notifications, not user text.
pub(super) fn job_event_entries(
    record: u64,
    block: usize,
    text: &str,
    projection: &Projection,
    view: &View,
    all: bool,
) -> Vec<Entry> {
    let kind = job_notification_kind(text);
    let legacy_agent_messages = matches!(kind, Some(JobNotificationKind::AgentMessage));
    let events = kind.and_then(|kind| {
        let (start, end) = kind.tags();
        text.trim()
            .strip_prefix(start)
            .and_then(|text| text.strip_suffix(end))
            .and_then(|json| serde_json::from_str::<Vec<Value>>(json).ok())
    });
    let Some(events) = events.filter(|events| !events.is_empty()) else {
        return vec![Entry::new(
            EntryKey::Notification {
                record,
                block,
                event: None,
            },
            "Job event · notification received by model · details unavailable".into(),
            Surface::Tool,
        )];
    };
    events
        .iter()
        .enumerate()
        .map(|(index, event)| {
            // Unified envelopes mix lifecycle and message items. The old envelope
            // remains message-only for historical sessions without a kind field.
            let agent_message = legacy_agent_messages
                || event.get("kind").and_then(Value::as_str) == Some("message");
            let key = EntryKey::Notification {
                record,
                block,
                event: Some(index),
            };
            let open = view.is_expanded(&key, all);
            let id = event
                .get("id")
                .and_then(Value::as_u64)
                .and_then(|id| JobId::new(id).ok());
            let job = id.and_then(|id| projection.jobs.get(&id));
            let tool = event
                .get("tool")
                .and_then(Value::as_str)
                .or_else(|| job.map(|job| job.tool.as_str()))
                .unwrap_or(if agent_message { "agent" } else { "job" });
            let state = if agent_message {
                event
                    .get("message")
                    .and_then(Value::as_u64)
                    .map_or_else(|| "message".into(), |message| format!("message #{message}"))
            } else {
                event
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or("updated")
                    .into()
            };
            let name = if agent_message {
                event
                    .get("name")
                    .and_then(Value::as_str)
                    .or_else(|| job.and_then(|job| job.name.as_deref()))
                    .map(|name| brief(&clean(name), 80))
            } else {
                None
            };
            let mut header = vec![
                Run::new(if open { "▾" } else { "▸" }, Role::Indicator),
                Run::new(" Job event", Role::Plain),
                Run::new(" · ", Role::Muted),
                Run::new(tool, Role::ToolName),
                Run::new(
                    id.map_or(String::new(), |id| format!(" #{id}")),
                    Role::Muted,
                ),
            ];
            if let Some(name) = name {
                header.push(Run::new(" · ", Role::Muted));
                header.push(Run::new(name, Role::Plain));
            }
            header.push(Run::new(" · ", Role::Muted));
            // Only the envelope's typed state is semantic, never message prose or
            // the job's current state (this notification is historical).
            let role = if agent_message {
                Role::Plain
            } else {
                serde_json::from_value::<JobState>(event["state"].clone())
                    .map(state_role)
                    .unwrap_or(Role::Plain)
            };
            header.push(Run::new(state, role));
            // A notification key is never a job key: output refresh must not
            // replace historical content with a live job card.
            let body = if open {
                let mut body = Document::default();
                if agent_message {
                    body.line("Agent message received by model", Role::Muted);
                    if let Some(text) = event.get("text").and_then(Value::as_str) {
                        body.line(clean(text), Role::Plain);
                    } else {
                        body.line("Message text unavailable", Role::Muted);
                    }
                } else {
                    body.line("Notification received by model", Role::Muted);
                    body.output(tool, job.map_or(&Value::Null, |job| &job.args), event);
                }
                Some(body)
            } else {
                None
            };
            Entry::card(key, header, body)
        })
        .collect()
}

/// A call without an admitted job uses the original response expansion key and
/// never acquires job navigation. The result is presentation-only session data.
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
mod tests {
    use super::super::JobInfo;
    use super::super::tests::{job_info, root};
    use super::*;

    fn events(text: &str, projection: &Projection, view: &View, all: bool) -> Vec<Entry> {
        job_event_entries(42, 0, text, projection, view, all)
    }

    #[test]
    fn notification_headers_use_only_historical_typed_states() {
        let text = format!(
            "<skyhook_job_events>{}</skyhook_job_events>",
            serde_json::json!([
                {"id": 1, "tool": "exec", "state": "failed"},
                {"id": 2, "tool": "exec", "state": "not failed but updated"},
                {"id": 3, "tool": "agent", "kind": "message", "name": "Failed", "message": 4, "state": "failed", "text": "Completed"}
            ])
        );
        let cards = events(&text, &Projection::default(), &View::default(), true);
        let expected = [
            ("▾ Job event · exec #1 · failed", Role::Error),
            (
                "▾ Job event · exec #2 · not failed but updated",
                Role::Plain,
            ),
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
        let text = format!(
            " <skyhook_agent_messages>\n{}\n</skyhook_agent_messages> ",
            serde_json::json!([{
                "id":253, "name":"implement-native-replay", "message":6577, "text":message,
            }]),
        );
        let mut projection = Projection::default();
        let mut view = View::default();
        let collapsed = events(&text, &projection, &view, false);
        assert_eq!(collapsed.len(), 1);
        let card = &collapsed[0];
        assert_eq!(
            (card.surface, card.expandable(), card.job_id()),
            (Surface::Tool, true, None)
        );
        let key = EntryKey::Notification {
            record: 42,
            block: 0,
            event: Some(0),
        };
        assert_eq!(card.key(), &key);
        let header = "Job event · agent #253 · implement-native-replay · message #6577";
        assert!(card.text().contains(header));
        assert!(!card.text().contains("skyhook_agent_messages") && !card.text().contains(message));
        view.set_expanded(key.clone(), true);
        let expanded = events(&text, &projection, &view, false);
        assert!(expanded[0].document().is_some());
        let expanded = expanded[0].text();
        assert!(expanded.contains(message));
        assert!(!expanded.contains("<skyhook_") && !expanded.contains("\\\"text\\\""));

        // A later terminal job snapshot must not relabel or replace the historical message.
        let job = JobInfo {
            name: Some("current name".into()),
            tool: "agent".into(),
            location: skyhook::execution::ExecutionLocation::named("host", ".".into()),
            ..job_info(
                &root(1),
                253,
                skyhook::job::JobRole::Agent,
                JobState::Completed,
            )
        };
        projection.jobs.insert(job.id, job);
        let after_completion = events(&text, &projection, &view, false);
        assert_eq!(after_completion[0].text(), expanded);
        assert!(after_completion[0].job_id().is_none());
        view.set_expanded(key, false);
        assert!(
            events(&text, &projection, &view, true)[0]
                .document()
                .is_none()
        );
    }

    #[test]
    fn unified_agent_message_preserves_legacy_expansion_and_attribution() {
        let payload = serde_json::json!([
            {"id":253,"name":"reviewer","message":6577,"text":"Historical reply.\nNext line."},
        ]);
        let legacy = format!("<skyhook_agent_messages>\n{payload}\n</skyhook_agent_messages>");
        let mut unified = payload;
        unified[0]["kind"] = serde_json::json!("message");
        let unified = format!("<skyhook_job_events>\n{unified}\n</skyhook_job_events>");
        let (projection, view) = (Projection::default(), View::default());
        for expanded in [false, true] {
            let legacy = events(&legacy, &projection, &view, expanded);
            let unified = events(&unified, &projection, &view, expanded);
            assert!(
                unified == legacy,
                "envelope migration changed message presentation"
            );
        }
    }

    #[test]
    fn malformed_agent_notifications_use_job_event_fallback_without_panicking() {
        for text in [
            "<skyhook_agent_messages>bad json</skyhook_agent_messages>",
            "<skyhook_agent_messages>[]</skyhook_agent_messages>",
            "<skyhook_agent_messages>[{}]",
        ] {
            assert!(job_notification_kind(text).is_some());
            let entries = events(text, &Projection::default(), &View::default(), true);
            assert_eq!((entries.len(), entries[0].surface), (1, Surface::Tool));
            let text = entries[0].text();
            assert!(text.contains("Job event") && text.contains("details unavailable"));
            assert!(!text.contains("<skyhook_"));
        }
    }
}

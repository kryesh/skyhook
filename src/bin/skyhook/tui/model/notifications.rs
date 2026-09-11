//! Historical job notifications, independent of the latest job state.

use super::super::format::brief;
use super::super::tool_view::{Document, Role, Run, Section};
use super::jobs::{header_text, state_role};
use super::{Entry, Projection, Surface, View, clean};
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
    key: &str,
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
            format!("{key}/events"),
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
            let key = format!("{key}/event{index}");
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
                    .map(|name| format!(" · {}", brief(&clean(name), 80)))
                    .unwrap_or_default()
            } else {
                String::new()
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
            if let Some(name) = name.strip_prefix(" · ") {
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
            let mut entry = Entry::new(key, header_text(&header), Surface::Tool);
            entry.header = Some(header.clone());
            entry.expandable = true;
            // Deliberately not Entry.job: output refresh must not replace this
            // historical notification with a live job card or discard its key.
            if open {
                let mut body = Document::default();
                body.sections.push(Section::Line(header));
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
                entry.text = body.plain_text();
                entry.document = Some(body);
            }
            entry
        })
        .collect()
}

/// A call without an admitted job uses the original response expansion key and
/// never acquires job navigation. The result is presentation-only session data.
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
mod tests {
    use super::super::JobInfo;
    use super::*;
    use skyhook::identity::{AgentId, SessionId};

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
        let cards = job_event_entries("m1", &text, &Projection::default(), &View::default(), true);
        let expected = [
            ("▾ Job event · exec #1 · failed", Role::Error),
            (
                "▾ Job event · exec #2 · not failed but updated",
                Role::Plain,
            ),
            ("▾ Job event · agent #3 · Failed · message #4", Role::Plain),
        ];
        for (card, (text, role)) in cards.iter().zip(expected) {
            let runs = card.header.as_ref().unwrap();
            assert_eq!(header_text(runs), text);
            assert_eq!(
                runs.last().unwrap(),
                &Run::new(text.rsplit(" · ").next().unwrap(), role)
            );
            assert_eq!(
                card.document.as_ref().unwrap().sections[0],
                Section::Line(runs.clone())
            );
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
        let collapsed = job_event_entries("m42/0", &text, &projection, &View::default(), false);
        assert_eq!(collapsed.len(), 1);
        let card = &collapsed[0];
        assert_eq!(card.surface, Surface::Tool);
        assert!(card.expandable);
        assert!(card.job.is_none());
        assert_eq!(card.key, "m42/0/event0");
        assert!(
            card.text
                .contains("Job event · agent #253 · implement-native-replay · message #6577")
        );
        assert!(!card.text.contains("skyhook_agent_messages"));
        assert!(!card.text.contains(message));
        let mut view = View::default();
        view.expanded.insert(card.key.clone());
        let expanded = job_event_entries("m42/0", &text, &projection, &view, false);
        assert!(expanded[0].document.is_some());
        assert!(expanded[0].text.contains(message));
        assert!(!expanded[0].text.contains("<skyhook_"));
        assert!(!expanded[0].text.contains("\\\"text\\\""));

        // A later terminal job snapshot must not relabel or replace the historical message.
        let id = JobId::new(253).unwrap();
        projection.jobs.insert(
            id,
            JobInfo {
                id,
                agent: AgentId::root(SessionId::from_bytes([1; 16])),
                name: Some("current name".into()),
                tool: "agent".into(),
                args: Value::Null,
                parent: None,
                state: JobState::Completed,
                target: "host".into(),
                location: ".".into(),
                remote: false,
                error: None,
            },
        );
        let after_completion = job_event_entries("m42/0", &text, &projection, &view, false);
        assert_eq!(after_completion[0].text, expanded[0].text);
        assert!(after_completion[0].job.is_none());
        view.collapsed.insert(card.key.clone());
        assert!(
            job_event_entries("m42/0", &text, &projection, &view, true)[0]
                .document
                .is_none()
        );
    }

    #[test]
    fn unified_agent_message_preserves_legacy_expansion_and_attribution() {
        let payload = serde_json::json!([
            {"id":253,"name":"reviewer","message":6577,"text":"Historical reply.\nNext line."},
        ]);
        let legacy = format!("<skyhook_agent_messages>\n{payload}\n</skyhook_agent_messages>");
        let mut unified_payload = payload;
        unified_payload[0]["kind"] = serde_json::json!("message");
        let unified = format!("<skyhook_job_events>\n{unified_payload}\n</skyhook_job_events>");
        let projection = Projection::default();
        for expanded in [false, true] {
            let view = View::default();
            let legacy = job_event_entries("m42/0", &legacy, &projection, &view, expanded);
            let unified = job_event_entries("m42/0", &unified, &projection, &view, expanded);
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
            let entries =
                job_event_entries("m1/0", text, &Projection::default(), &View::default(), true);
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].surface, Surface::Tool);
            assert!(entries[0].text.contains("Job event"));
            assert!(entries[0].text.contains("details unavailable"));
            assert!(!entries[0].text.contains("<skyhook_"));
        }
    }
}

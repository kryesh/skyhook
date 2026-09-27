//! Historical job notifications, independent of the latest job state.

use super::super::tool_view::{Document, Hints, Role, Run};
use super::jobs::{HEADER_DETAIL, state_name, state_role};
use super::{Entry, EntryKey, Projection, View, clean};
use crate::text::brief;
use serde_json::Value;
use skyhook::job::JobRole;
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
                Run::new("Job event", Role::Plain),
                Run::new(" · ", Role::Muted),
            ];
            // A notification key is never a job key: output refresh must not
            // replace historical content with a live job card.
            let mut body = open.then(Document::default);
            match event {
                JobEvent::Message(message) => {
                    let job = projection.jobs().get(&message.id);
                    let tool = job.map_or(JobRole::Agent.as_str(), |job| job.tool.as_str());
                    header.push(Run::new(tool, Role::ToolName));
                    header.push(Run::new(format!(" #{}", message.id), Role::Muted));
                    let name = message
                        .name
                        .as_ref()
                        .map(|name| name.as_str())
                        .or_else(|| job.and_then(|job| job.name.as_deref()));
                    if let Some(name) = name {
                        header.push(Run::new(" · ", Role::Muted));
                        header.push(Run::new(brief(&clean(name), HEADER_DETAIL), Role::Plain));
                    }
                    header.push(Run::new(" · ", Role::Muted));
                    header.push(Run::new(
                        format!("message #{}", message.message),
                        Role::Plain,
                    ));
                    if let Some(body) = &mut body {
                        body.line("Agent message received by model", Role::Muted);
                        body.line(message.text.as_str(), Role::Plain);
                    }
                }
                JobEvent::Job(job_view) => {
                    let id = job_view.id();
                    let job = id.and_then(|id| projection.jobs().get(&id));
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
                    header.push(Run::new(state_name(state), state_role(state)));
                    if let Some(body) = &mut body {
                        body.line("Notification received by model", Role::Muted);
                        let args = job.map_or(&Value::Null, |job| &job.args);
                        body.output(Hints::new(tool, args), Some(job_view), &[]);
                    }
                }
            }
            Entry::card(key, header, body)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Journal, created, finished};
    use super::*;
    use crate::tui::model::Surface;
    use skyhook::execution::ExecutionLocation;
    use skyhook::session::{RecordSeq, SessionEvent};

    fn events(events: &[JobEvent], projection: &Projection, view: &View, all: bool) -> Vec<Entry> {
        job_event_entries(RecordSeq::default(), 0, events, projection, view, all)
    }

    #[tokio::test]
    async fn intermediate_agent_messages_are_historical_expandable_job_events() {
        let message = "Both reviewer gaps are fixed.\nChecking \"native\" replay — next.";
        let reply = [JobEvent::Message(skyhook::job::AgentMessage {
            id: skyhook::identity::JobId::new(253).unwrap(),
            name: Some("implement-native-replay".parse().unwrap()),
            message: RecordSeq::default().message(),
            text: message.into(),
        })];
        let (mut journal, mut projection) = (Journal::new().await, Projection::default());
        let agent = journal.agent();
        let mut view = View::default();
        let collapsed = events(&reply, &projection, &view, false);
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
        let header = "Job event · agent #253 · implement-native-replay · message #0";
        assert!(card.text().contains(header));
        assert!(!card.text().contains("skyhook_job_events") && !card.text().contains(message));
        view.set_expanded(key.clone(), true);
        let expanded = events(&reply, &projection, &view, false);
        assert!(expanded[0].document().is_some());
        let expanded = expanded[0].text();
        assert!(expanded.contains(message));
        assert!(!expanded.contains("<skyhook_") && !expanded.contains("\\\"text\\\""));

        // A later terminal job snapshot must not relabel or replace the historical message.
        let mut created = created(253, "agent", JobRole::Agent);
        if let SessionEvent::JobCreated { name, location, .. } = &mut created {
            *name = Some("current-name".parse().unwrap());
            *location = ExecutionLocation::named("host".parse().unwrap(), ".".into());
        }
        journal.record(&agent, created).await;
        journal.record(&agent, finished(253)).await;
        projection.rebuild(&journal.snapshot);
        let after_completion = events(&reply, &projection, &view, false);
        assert_eq!(after_completion[0].text(), expanded);
        assert!(after_completion[0].job_id().is_none());
        view.set_expanded(key, false);
        assert!(
            events(&reply, &projection, &view, true)[0]
                .document()
                .is_none()
        );
    }
}

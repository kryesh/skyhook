//! Job notifications as the model received them, headed by the job's current state.

use super::super::tool_view::{Document, Hints, Role, Run};
use super::entries::Inputs;
use super::jobs::{HEADER_DETAIL, state_name, state_role};
use super::{Entry, EntryKey, View, clean};
use crate::text::brief;
use serde_json::Value;
use skyhook::job::JobRole;
use skyhook::session::{JobEvent, RecordSeq};

/// Show the events where the model received them, using their historical payload
/// rather than the job's latest output (the same job may have since resumed). A
/// job card's header shows the job's current state, so a question it asked reads
/// as answered once the job moves on.
pub(super) fn job_event_entries(
    record: RecordSeq,
    block: usize,
    events: &[JobEvent],
    inputs: Inputs<'_>,
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
                    let job = inputs.job(&message.id);
                    let tool = job.map_or(JobRole::Agent.as_str(), |job| job.tool.as_str());
                    header.push(Run::new(tool, Role::ToolName));
                    header.push(Run::new(format!(" #{}", message.id), Role::Muted));
                    let name = (message.name.as_ref())
                        .or_else(|| job.and_then(|job| job.name.as_ref()))
                        .map(|name| name.as_str());
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
                    let id = job_view.id;
                    let job = id.and_then(|id| inputs.job(&id));
                    let tool = job_view
                        .tool()
                        .or_else(|| job.map(|job| job.tool.as_str()))
                        .unwrap_or("job");
                    let state = job.map_or(job_view.state(), |job| job.state);
                    header.push(Run::new(tool, Role::ToolName));
                    header.push(Run::new(
                        id.map_or(String::new(), |id| format!(" #{id}")),
                        Role::Muted,
                    ));
                    header.push(Run::new(" · ", Role::Muted));
                    header.push(Run::new(state_name(state), state_role(state)));
                    if let Some(body) = &mut body {
                        body.line("Notification received by model", Role::Muted);
                        let args = job.map_or(&Value::Null, |job| &job.arguments);
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
    use super::*;
    use crate::tui::app::played::{
        answer_child, asking_child, job_finished, job_of_call, observe_find, observe_record,
        played_fixture, reached,
    };
    use crate::tui::model::Surface;
    use serde_json::json;
    use skyhook::job::JobTransition;
    use skyhook::session::RecordSeq;

    use super::super::{EntryView, Projection, Tab, Window, tests::root};
    use crate::tui::app::OutputStore;
    use skyhook::agent::ObservationSnapshot;

    /// The cards of `events` for jobs as `live` shows them.
    fn events(
        events: &[JobEvent],
        live: &ObservationSnapshot,
        view: &View,
        all: bool,
    ) -> Vec<Entry> {
        let agent = root(1);
        let window = Window::empty(agent.clone(), Tab::Conversation, 0);
        let (projection, outputs) = (Projection::default(), OutputStore::default());
        let presentation = EntryView {
            agent: &agent,
            tab: Tab::Conversation,
            view,
            all_details: all,
            today: Default::default(),
        };
        let inputs = Inputs {
            live,
            window: &window,
            projection: &projection,
            presentation,
            outputs: &outputs,
            unsaved: &[],
        };
        job_event_entries(RecordSeq::default(), 0, events, inputs, view, all)
    }

    #[tokio::test]
    async fn intermediate_agent_messages_are_historical_expandable_job_events() {
        let (_root, mut app, mut model, session) = played_fixture().await;
        let task = json!({"prompt": "ask", "name": "current-name"});
        asking_child(&session, &mut model, task).await;
        let job = observe_find(&mut app, |app| job_of_call(&app.journal, "agent-0")).await;
        reached(&mut app, job, JobTransition::WaitingInput).await;
        let message = "Both reviewer gaps are fixed.\nChecking \"native\" replay — next.";
        let reply = [JobEvent::Message(skyhook::job::AgentMessage {
            id: job,
            name: Some("implement-native-replay".parse().unwrap()),
            message: RecordSeq::default().message(),
            text: message.into(),
        })];
        let mut view = View::default();
        let unknown = ObservationSnapshot::default();
        let collapsed = events(&reply, &unknown, &view, false);
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
        let header = format!("Job event · agent #{job} · implement-native-replay · message #0");
        assert!(card.text().contains(&header));
        assert!(!card.text().contains("skyhook_job_events") && !card.text().contains(message));
        view.set_expanded(key.clone(), true);
        let expanded = events(&reply, &unknown, &view, false);
        assert!(expanded[0].document().is_some());
        let expanded = expanded[0].text();
        assert!(expanded.contains(message));
        assert!(!expanded.contains("<skyhook_") && !expanded.contains("\\\"text\\\""));

        // A job card is headed by the job's current state over the question it asked.
        let question = json!({"id": job, "state": "waiting_input",
            "presentation": {"question": {"questions": [{"id": "pick", "prompt": "Which one?"}]}}});
        let question = [JobEvent::Job(serde_json::from_value(question).unwrap())];
        let headed = |snapshot: &ObservationSnapshot, state: &str| {
            let text = events(&question, snapshot, &view, false)[0]
                .text()
                .to_owned();
            assert!(
                text.contains(&format!("agent #{job} · {state}\n")),
                "{text}"
            );
            assert!(text.contains("Which one?"));
        };
        headed(&app.snapshot, "Waiting for input");
        answer_child(&session, job, "{pick: 'this'}").await;
        model.turn().await.answer("done");
        observe_record(&mut app, job_finished(job)).await;
        headed(&app.snapshot, "Completed");
        // Its terminal snapshot, under another name, must not relabel or replace
        // the historical message.
        let after_completion = events(&reply, &app.snapshot, &view, false);
        assert_eq!(after_completion[0].text(), expanded);
        assert!(after_completion[0].job_id().is_none());
        view.set_expanded(key, false);
        assert!(
            events(&reply, &app.snapshot, &view, true)[0]
                .document()
                .is_none()
        );
    }
}

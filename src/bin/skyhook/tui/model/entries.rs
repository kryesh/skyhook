//! Tab selection and ordered conversation projection, including call/result provenance.

use crate::tui::app::OutputStore;

use super::jobs::{call_entry, job_entry};
use super::live::{
    reasoning_entry, reasoning_key, response_block_key, response_entries, working_entry,
};
use super::notifications::{is_job_notification, job_event_entries};
use super::requests::{request_entry, request_running};
use super::{Entry, EntryKey, EntryView, Projection, Surface, Tab, number, pretty};
use skyhook::agent::ObservationSnapshot;
use skyhook::identity::JobId;
use skyhook::provider::protocol::{BlockContent, Message, UserContent};
use skyhook::session::SessionEvent;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolGroup {
    Response(u64),
    Script(JobId),
    Notification(u64, usize),
}

/// Shared by retained UI content and fresh export construction.
pub fn entries(
    snapshot: &ObservationSnapshot,
    projection: &Projection,
    presentation: EntryView<'_>,
    outputs: &OutputStore,
    include_live: bool,
) -> Vec<Entry> {
    let EntryView {
        agent,
        view,
        thinking,
        all_details,
    } = presentation;
    let records: Vec<_> = projection
        .records_by_agent
        .get(agent)
        .into_iter()
        .flatten()
        .filter_map(|sequence| snapshot.records.get(sequence))
        .collect();
    match view.tab {
        Tab::Requests => records
            .iter()
            .filter_map(|r| {
                if let SessionEvent::ModelRequested { purpose, .. } = &r.event {
                    let info = projection.requests.get(&r.sequence)?;
                    let running = request_running(info, snapshot, projection, agent, r.sequence);
                    Some(request_entry(r.sequence, purpose, info, running))
                } else {
                    None
                }
            })
            .collect(),
        Tab::Jobs => projection
            .jobs
            .values()
            .filter(|j| &j.agent == agent)
            .map(|job| {
                let mut entry = job_entry(job, projection, view, outputs, all_details);
                // The Jobs tab is a dense list; conversation grouping owns its
                // spacing separately, and expanded documents stay unchanged.
                entry.compact_after = true;
                entry
            })
            .collect(),
        Tab::Conversation => {
            // Tool results have an explicit call ID but no message sequence. Resolve
            // only within the current agent's most recent assistant turn, consuming
            // each call once. Never let an old/reused ID suppress a later result.
            let mut pending = HashMap::new();
            let mut turn = None;
            let mut call_results = HashMap::new();
            let mut matched_results = HashSet::new();
            for record in &records {
                match &record.event {
                    SessionEvent::MessageCommitted {
                        message: Message::Assistant(items),
                    } => {
                        pending.clear();
                        turn = Some(record.sequence);
                        for block in items.iter().flat_map(|item| &item.blocks) {
                            if let BlockContent::ToolCall(call) = &block.content {
                                pending.insert(
                                    (call.id().to_owned(), call.name().to_owned()),
                                    record.sequence,
                                );
                            }
                        }
                    }
                    SessionEvent::JobCreated {
                        origin: Some(origin),
                        tool,
                        ..
                    } if turn.is_none_or(|turn| turn <= origin.message) => {
                        // Retained job provenance also identifies a call whose
                        // assistant message is no longer in the retained history.
                        pending.insert((origin.call_id.clone(), tool.clone()), origin.message);
                        turn = Some(origin.message);
                    }
                    SessionEvent::MessageCommitted {
                        message: Message::Tool(results),
                    } => {
                        for (index, result) in results.iter().enumerate() {
                            if let Some(message) =
                                pending.remove(&(result.call_id.clone(), result.name.clone()))
                            {
                                call_results.insert((message, result.call_id.clone()), result);
                                matched_results.insert((record.sequence, index));
                            }
                        }
                    }
                    _ => {}
                }
            }
            let mut entries = Vec::new();
            let mut tool_groups = HashMap::new();
            let agent_name = projection.agent_name(agent);
            for record in &records {
                let key = EntryKey::Record(record.sequence);
                match &record.event {
                    SessionEvent::ModelRequested { .. } => {
                        if let Some(entry) = super::retry::retry_entry(
                            snapshot,
                            projection,
                            agent,
                            record.sequence,
                            thinking,
                        ) {
                            entries.push(entry);
                            continue;
                        }
                        if let Some(response) =
                            snapshot.responses.get(&(agent.clone(), record.sequence))
                            && response.settled
                            && !projection.live_response(record.sequence, response)
                            && !projection.response_committed(record.sequence)
                        {
                            entries.extend(response_entries(
                                record.sequence,
                                response,
                                view,
                                thinking,
                                agent_name,
                            ));
                        }
                    }
                    SessionEvent::MessageCommitted { message } => match message {
                        Message::User(blocks) => {
                            for (i, block) in blocks.iter().enumerate() {
                                let (text, surface) = match block {
                                    UserContent::Text { text } => (
                                        format!(
                                            "{}\n{text}",
                                            if agent.path().is_empty() {
                                                "You"
                                            } else {
                                                "Parent"
                                            }
                                        ),
                                        Surface::User,
                                    ),
                                    UserContent::ParentInput { text } => {
                                        (format!("Parent\n{text}"), Surface::User)
                                    }
                                    UserContent::Attachment { attachment } => (
                                        format!("Attachment\n{}", pretty(attachment)),
                                        Surface::User,
                                    ),
                                    UserContent::Runtime { text } if is_job_notification(text) => {
                                        for entry in job_event_entries(
                                            record.sequence,
                                            i,
                                            text,
                                            projection,
                                            view,
                                            all_details,
                                        ) {
                                            tool_groups.insert(
                                                entry.key().clone(),
                                                ToolGroup::Notification(record.sequence, i),
                                            );
                                            entries.push(entry);
                                        }
                                        continue;
                                    }
                                    // Persisted runtime state is model context, not conversation.
                                    UserContent::Runtime { text }
                                        if text.starts_with("<skyhook_state>") =>
                                    {
                                        continue;
                                    }
                                    UserContent::Runtime { text } => {
                                        (format!("Harness notification\n{text}"), Surface::Muted)
                                    }
                                    UserContent::Compaction { text } => {
                                        (format!("Compaction\n{text}"), Surface::Muted)
                                    }
                                };
                                entries.push(Entry::new(
                                    EntryKey::UserBlock {
                                        record: record.sequence,
                                        index: i,
                                    },
                                    text,
                                    surface,
                                ));
                            }
                        }
                        Message::Assistant(items) => {
                            let request = projection
                                .response_requests
                                .get(&record.sequence)
                                .copied()
                                .unwrap_or(record.sequence);
                            let blocks: Vec<_> = items
                                .iter()
                                .flat_map(|item| item.blocks.iter().map(move |block| (item, block)))
                                .collect();
                            let final_text = if blocks.iter().any(|(_, block)| {
                                matches!(&block.content, BlockContent::ToolCall(_))
                            }) {
                                None
                            } else {
                                blocks.iter().rposition(|(_, block)| {
                                    matches!(&block.content, BlockContent::Text { text } if !text.trim().is_empty())
                                })
                            };
                            for (i, (item, block)) in blocks.iter().enumerate() {
                                let block_key = response_block_key(request, &item.id, &block.id);
                                match &block.content {
                                    BlockContent::Text { text } if !text.trim().is_empty() => {
                                        let mut entry = Entry::new(
                                            block_key.clone(),
                                            format!("{agent_name}\n{text}"),
                                            Surface::Agent,
                                        );
                                        if Some(i) == final_text {
                                            entry.footer = projection
                                                .response_requests
                                                .get(&record.sequence)
                                                .and_then(|request| {
                                                    projection.requests.get(request)
                                                })
                                                .and_then(|request| request.start.as_ref())
                                                .and_then(|start| start.model.clone());
                                        }
                                        entries.push(entry);
                                    }
                                    BlockContent::Reasoning { text, .. }
                                        if !text.trim().is_empty() =>
                                    {
                                        entries.push(reasoning_entry(
                                            reasoning_key(request, &item.id, &block.id),
                                            text,
                                            view,
                                            thinking,
                                            super::live::ReasoningStatus::Complete,
                                        ));
                                    }
                                    BlockContent::ToolCall(call) => {
                                        let exists = projection.tool_origins.contains(&(
                                            agent.clone(),
                                            record.sequence,
                                            call.id().to_owned(),
                                        ));
                                        if !exists {
                                            let e = call_entry(
                                                block_key.clone(),
                                                (
                                                    call.name(),
                                                    Some(call.arguments()),
                                                    call_results
                                                        .get(&(
                                                            record.sequence,
                                                            call.id().to_owned(),
                                                        ))
                                                        .copied(),
                                                ),
                                                agent,
                                                projection,
                                                view.is_expanded(&block_key, all_details),
                                            );
                                            tool_groups.insert(
                                                e.key().clone(),
                                                ToolGroup::Response(record.sequence),
                                            );
                                            entries.push(e);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Message::Tool(results) => {
                            for (index, result) in results.iter().enumerate() {
                                if !matched_results.contains(&(record.sequence, index)) {
                                    let result_key = EntryKey::ToolResult {
                                        record: record.sequence,
                                        call: result.call_id.clone(),
                                    };
                                    let open = view.is_expanded(&result_key, all_details);
                                    entries.push(call_entry(
                                        result_key,
                                        (result.name.as_str(), None, Some(result)),
                                        agent,
                                        projection,
                                        open,
                                    ));
                                }
                            }
                        }
                    },
                    SessionEvent::JobCreated { job, origin, .. } => {
                        if let Some(job) = projection.jobs.get(job) {
                            let entry = job_entry(job, projection, view, outputs, all_details);
                            // Script children belong to their immediate script,
                            // not to the model response that launched the script.
                            let group = job
                                .parent
                                .and_then(|parent| projection.jobs.get(&parent))
                                .filter(|parent| parent.role == skyhook::job::JobRole::Script)
                                .map(|parent| ToolGroup::Script(parent.id))
                                .or_else(|| {
                                    origin
                                        .as_ref()
                                        .map(|origin| ToolGroup::Response(origin.message))
                                });
                            if let Some(group) = group {
                                tool_groups.insert(entry.key().clone(), group);
                            }
                            entries.push(entry);
                        }
                    }
                    SessionEvent::Compaction { checkpoint } => {
                        let key = EntryKey::Record(record.sequence);
                        let open = view.is_expanded(&key, false);
                        let e = Entry::expandable_text(
                            key,
                            format!(
                                "{} Context compacted · {} → {}{}",
                                if open { "▾" } else { "▸" },
                                number(checkpoint.before_tokens),
                                number(checkpoint.after_tokens),
                                if open {
                                    format!(
                                        "\nSummary and retained sources\n{}",
                                        pretty(checkpoint)
                                    )
                                } else {
                                    String::new()
                                }
                            ),
                            Surface::Muted,
                        );
                        entries.push(e);
                    }
                    SessionEvent::Status { message } => entries.push(Entry::new(
                        key,
                        format!("Status · {message}"),
                        Surface::Status,
                    )),
                    SessionEvent::CompactionFailed { error, .. } => entries.push(Entry::new(
                        key,
                        format!("Compaction failed; previous context retained\n{error}"),
                        Surface::Error,
                    )),
                    SessionEvent::CompactionSkipped { reason, .. } => entries.push(Entry::new(
                        key,
                        format!("Compaction skipped · {reason}"),
                        Surface::Muted,
                    )),
                    _ => {}
                }
            }
            // Store adjacency on the preceding entry so equality-based cache
            // invalidation also relayouts it when a sibling arrives or disappears.
            for index in 0..entries.len().saturating_sub(1) {
                let next_group = tool_groups.get(entries[index + 1].key());
                let script_child = entries[index]
                    .job_id()
                    .is_some_and(|job| next_group == Some(&ToolGroup::Script(job)));
                entries[index].compact_after = script_child
                    || tool_groups
                        .get(entries[index].key())
                        .is_some_and(|group| next_group == Some(group));
            }
            if !include_live {
                return entries;
            }
            let responses = super::live::live_tail_responses(snapshot, projection, agent);
            entries.extend(responses.into_iter().flat_map(|(request, response)| {
                response_entries(request, response, view, thinking, agent_name)
            }));
            if let Some(entry) = working_entry(
                snapshot,
                projection,
                agent,
                entries.iter().any(|entry| entry.running),
            ) {
                entries.push(entry);
            }
            entries
        }
    }
}
#[cfg(test)]
mod tests {
    use super::super::View;
    use super::super::tests::{
        call_record, delta, record, replay, request, result_record, root, update,
    };
    use super::*;
    use skyhook::agent::{AgentActivity, RuntimeEvent};
    use skyhook::identity::AgentId;
    use skyhook::provider::protocol::{AssistantItem, ToolCall};

    fn render(snapshot: &ObservationSnapshot, agent: &AgentId, details: bool) -> Vec<Entry> {
        render_with(snapshot, agent, false, details)
    }

    fn render_with(
        snapshot: &ObservationSnapshot,
        agent: &AgentId,
        thinking: bool,
        details: bool,
    ) -> Vec<Entry> {
        let mut projection = Projection::default();
        projection.rebuild(snapshot);
        let (view, outputs) = (View::default(), OutputStore::default());
        let view = EntryView {
            agent,
            view: &view,
            thinking,
            all_details: details,
        };
        entries(snapshot, &projection, view, &outputs, true)
    }

    fn commit(snapshot: &mut ObservationSnapshot, agent: &AgentId, message: Message) -> u64 {
        record(snapshot, agent, SessionEvent::MessageCommitted { message })
    }

    fn user(text: &str) -> Message {
        Message::User(vec![UserContent::Text { text: text.into() }])
    }

    #[test]
    fn synchronous_results_are_turn_scoped_and_orphans_remain_expandable() {
        let agent = root(72);
        let mut snapshot = ObservationSnapshot::default();
        for error in [false, true] {
            call_record(&mut snapshot, &agent, "reused");
            result_record(&mut snapshot, &agent, "reused", error);
        }
        result_record(&mut snapshot, &agent, "orphan", true);

        let cards = render(&snapshot, &agent, true);
        assert_eq!(cards.len(), 3);
        assert!(cards[0].text().starts_with("▾ ✓ exec · Completed"));
        assert!(
            cards[1..]
                .iter()
                .all(|card| card.text().starts_with("▾ × exec · Failed"))
        );
        assert_ne!(cards[0].key(), cards[1].key());
        assert!(!cards[2].text().contains("Arguments"));
        assert!(
            cards[2].text().contains("Output") && cards[2].text().contains("permission_denied")
        );
        assert!(cards.iter().all(|card| card.expandable()
            && card.job_id().is_none()
            && card.surface == Surface::Tool));

        // A new assistant turn closes the old call scope even if its old call
        // never produced a result. Same-ID results in other agents cannot bind.
        let other = agent.child(1);
        result_record(&mut snapshot, &other, "reused", true);
        call_record(&mut snapshot, &agent, "pending");
        call_record(&mut snapshot, &agent, "new-turn");
        result_record(&mut snapshot, &agent, "pending", true);
        let cards = render(&snapshot, &agent, false);
        let texts: Vec<_> = cards.iter().map(Entry::text).collect();
        assert_eq!(texts[3..], ["▸ exec", "▸ exec", "▸ × exec · Failed"]);
        assert_eq!(render(&snapshot, &other, false).len(), 1);
    }

    #[test]
    fn admitted_results_use_exact_job_provenance_even_without_retained_calls() {
        use skyhook::{execution::ExecutionLocation, session::ModelCallOrigin};
        for retained in [false, true] {
            let agent = root(73);
            let mut snapshot = ObservationSnapshot::default();
            let origin = call_record(&mut snapshot, &agent, "reused");
            let created = SessionEvent::JobCreated {
                job: JobId::new(42).unwrap(),
                parent: None,
                origin: Some(ModelCallOrigin {
                    message: origin,
                    call_id: "reused".into(),
                }),
                tool: "exec".into(),
                role: skyhook::job::JobRole::Tool,
                name: None,
                arguments: serde_json::json!({"argv": ["echo"]}),
                output_schema: None,
                accepts_input: false,
                background: false,
                authorization_scope: None,
                location: ExecutionLocation::root("/workspace".into()),
            };
            record(&mut snapshot, &agent, created);
            result_record(&mut snapshot, &agent, "reused", false);
            if !retained {
                snapshot.records.remove(&origin);
            }
            // A later call reuses the ID but fails before creating a job.
            call_record(&mut snapshot, &agent, "reused");
            result_record(&mut snapshot, &agent, "reused", true);

            let cards = render(&snapshot, &agent, false);
            assert_eq!(cards.len(), 2);
            assert_eq!(cards[0].job_id(), Some(JobId::new(42).unwrap()));
            assert_eq!(
                (cards[1].job_id(), cards[1].text()),
                (None, "▸ × exec · Failed")
            );
        }
    }

    #[test]
    fn whitespace_only_turns_neither_create_agent_cards_nor_steal_the_answer_footer() {
        let agent = root(3);
        let call = ToolCall::new("call_read", "read", serde_json::json!({"path":"."})).unwrap();
        for whitespace in ["\n\n", "\n\n\n", " \t\r\n", "\u{2003}"] {
            let mut snapshot = ObservationSnapshot::default();
            request(&mut snapshot, &agent, None);
            let reasoning = "Inspect the repository.";
            let message = Message::Assistant(vec![
                AssistantItem::reasoning("reasoning", 0, reasoning, Some(replay())),
                AssistantItem::text("separator", 1, whitespace),
                AssistantItem::tool_call("tool", 2, call.clone()),
            ]);
            commit(&mut snapshot, &agent, message);

            let cards = render(&snapshot, &agent, true);
            assert!(cards.iter().all(|card| card.surface != Surface::Agent));
            assert!(
                cards
                    .iter()
                    .any(|card| card.surface == Surface::Reasoning
                        && card.text().contains(reasoning))
            );
            assert!(
                cards
                    .iter()
                    .any(|card| card.surface == Surface::Tool && card.text().contains("read"))
            );
        }

        let mut snapshot = ObservationSnapshot::default();
        request(&mut snapshot, &agent, None);
        let answer = "  Actual answer with spacing.\n";
        let message = Message::Assistant(vec![
            AssistantItem::text("answer", 0, answer),
            AssistantItem::text("separator", 1, "\n\n"),
        ]);
        commit(&mut snapshot, &agent, message);
        let cards = render(&snapshot, &agent, false);
        let answers: Vec<_> = cards
            .iter()
            .filter(|card| card.surface == Surface::Agent)
            .collect();
        assert_eq!(answers.len(), 1);
        assert!(answers[0].text().ends_with(answer));
        assert_eq!(answers[0].footer.as_deref(), Some("fixture-model"));
    }

    #[test]
    fn conversation_projects_mixed_job_events_without_reclassifying_user_text() {
        const RECEIVED: &str = "Agent message received by model";
        let agent = root(2);
        let messages = format!(
            "<skyhook_job_events>\n{}\n</skyhook_job_events>",
            serde_json::json!([
                {"kind":"message","id":253,"message":6577,"text":"first progress"},
                {"kind":"message","id":253,"message":6578,"text":"second progress"},
            ]),
        );
        let jobs = format!(
            "<skyhook_job_events>\n{}\n</skyhook_job_events>",
            serde_json::json!([
                {"kind":"message","id":253,"name":"reviewer","message":6579,"text":"independent reply"},
                {"id":253,"tool":"agent","state":"completed","last_message":6579},
                {"kind":"message","id":254,"message":6580,"text":"another child reply"},
                {"id":255,"tool":"exec","state":"completed","result":{"stdout":"tool output"}},
            ]),
        );
        let message = Message::User(vec![
            UserContent::Runtime {
                text: messages.clone(),
            },
            UserContent::Runtime { text: jobs },
            UserContent::Text {
                text: messages.clone(),
            },
            UserContent::Runtime {
                text: "ordinary scheduler note".into(),
            },
        ]);
        // Saved histories retain the original runtime envelopes; rendering is presentation-only.
        let saved = serde_json::to_vec(&message).unwrap();
        let mut snapshot = ObservationSnapshot::default();
        let sequence = commit(
            &mut snapshot,
            &agent,
            serde_json::from_slice(&saved).unwrap(),
        );

        let cards = render(&snapshot, &agent, true);
        let notifications: Vec<_> = cards
            .iter()
            .filter(|entry| entry.surface == Surface::Tool)
            .collect();
        let expected: [(usize, usize, &[&str], &[&str]); 6] = [
            (0, 0, &["first progress"], &[]),
            (0, 1, &["second progress"], &[]),
            (
                1,
                0,
                &[
                    "agent #253 · reviewer · message #6579",
                    RECEIVED,
                    "independent reply",
                ],
                &[],
            ),
            (
                1,
                1,
                &["agent #253 · completed"],
                &[RECEIVED, "independent reply"],
            ),
            (
                1,
                2,
                &[
                    "agent #254 · message #6580",
                    RECEIVED,
                    "another child reply",
                ],
                &[],
            ),
            (1, 3, &["exec #255 · completed", "tool output"], &[RECEIVED]),
        ];
        assert_eq!(notifications.len(), expected.len());
        for (entry, (block, event, present, absent)) in notifications.iter().zip(expected) {
            let key = EntryKey::Notification {
                record: sequence,
                block,
                event: Some(event),
            };
            let text = entry.text();
            assert_eq!(entry.key(), &key);
            assert!(present.iter().all(|part| text.contains(part)), "{text}");
            assert!(!absent.iter().any(|part| text.contains(part)), "{text}");
            assert!(entry.expandable() && entry.job_id().is_none());
            assert!(!text.contains("<skyhook_") && !text.contains("Harness notification"));
        }
        assert!(notifications[0].compact_after && !notifications[1].compact_after);
        let user = format!("You\n{messages}");
        assert!(
            cards
                .iter()
                .any(|entry| entry.surface == Surface::User && entry.text() == user)
        );
        assert!(cards.iter().any(|entry| entry.surface == Surface::Muted
            && entry.text() == "Harness notification\nordinary scheduler note"));
        let SessionEvent::MessageCommitted { message: stored } = &snapshot.records[&sequence].event
        else {
            panic!("message history changed during rendering");
        };
        assert_eq!(serde_json::to_vec(stored).unwrap(), saved);
    }

    #[test]
    fn refusals_reach_the_transcript_as_errors_through_the_real_projection() {
        // Guards the journal-to-card wiring: rendering a refusal as an ordinary
        // failure would restore the silent-failure UX this classification exists
        // to prevent.
        let mut snapshot = ObservationSnapshot::default();
        let agent = root(1);
        let refused_request = request(&mut snapshot, &agent, None);
        let error = "the model declined to respond: content filter; \
                     the response contained no content"
            .to_string();
        let refused = SessionEvent::ModelFailed {
            request: refused_request,
            attempt: 1,
            error: error.clone(),
            kind: skyhook::session::ModelFailureKind::Refusal,
        };
        record(&mut snapshot, &agent, refused);
        let entries = render(&snapshot, &agent, false);
        let card = entries
            .iter()
            .find(|entry| entry.key() == &EntryKey::Retry(refused_request))
            .expect("a refusal is presented in the conversation");
        assert_eq!(card.surface, Surface::Error);
        let mut lines = card.text().lines();
        assert_eq!(lines.next(), Some("Model declined to respond · attempt 1"));
        assert_eq!(lines.next(), Some(error.as_str()));
        assert_eq!(lines.next(), Some(super::super::retry::REFUSAL_HINT));
        assert_eq!(lines.next(), None);
        // An ordinary failure keeps the muted presentation and gains no hint.
        let mut snapshot = ObservationSnapshot::default();
        let failed_request = request(&mut snapshot, &agent, None);
        let failed = SessionEvent::ModelFailed {
            request: failed_request,
            attempt: 1,
            error: "Protocol: rejected".to_owned(),
            kind: skyhook::session::ModelFailureKind::Error,
        };
        record(&mut snapshot, &agent, failed);
        let entries = render(&snapshot, &agent, false);
        let card = entries
            .iter()
            .find(|entry| entry.key() == &EntryKey::Retry(failed_request))
            .expect("an ordinary failure is still presented");
        assert_eq!(card.surface, Surface::Status);
        assert_eq!(
            card.text(),
            "Request failed · attempt 1\nProtocol: rejected"
        );
    }

    #[test]
    fn partial_attempts_stay_before_retry_and_followup_while_current_stream_stays_last() {
        let mut snapshot = ObservationSnapshot::default();
        let agent = root(1);
        let failed = request(&mut snapshot, &agent, None);
        let context = Some(failed - 1);
        delta(&mut snapshot, &agent, failed, "text", "failed partial");
        let error = "stream lost".into();
        let event = SessionEvent::ModelFailed {
            request: failed,
            attempt: 1,
            error,
            kind: skyhook::session::ModelFailureKind::Error,
        };
        record(&mut snapshot, &agent, event);
        let retry = request(&mut snapshot, &agent, context);
        let answer = AssistantItem::text("text", 1, "successful retry");
        commit(&mut snapshot, &agent, Message::Assistant(vec![answer]));
        commit(&mut snapshot, &agent, user("new followup"));
        let interrupted = request(&mut snapshot, &agent, context);
        delta(
            &mut snapshot,
            &agent,
            interrupted,
            "text",
            "interrupted partial",
        );
        let activity = AgentActivity::Interrupted;
        let event = RuntimeEvent::Activity {
            agent: agent.clone(),
            activity,
        };
        update(&mut snapshot, event);
        let current = request(&mut snapshot, &agent, context);
        delta(&mut snapshot, &agent, current, "text", "current stream");
        delta(&mut snapshot, &agent.child(1), retry, "text", "child only");

        let entries = render(&snapshot, &agent, false);
        let text = entries
            .iter()
            .map(Entry::text)
            .collect::<Vec<_>>()
            .join("\n");
        let order = [
            "failed partial",
            "successful retry",
            "new followup",
            "interrupted partial",
            "current stream",
        ];
        for pair in order.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            assert!(
                text.find(a).unwrap() < text.find(b).unwrap(),
                "{a} must precede {b}: {text}"
            );
        }
        assert_eq!(text.matches("failed partial").count(), 1);
        assert!(!text.contains("child only"));
        assert!(entries.last().unwrap().text().contains("current stream"));
    }

    #[test]
    fn ordinary_conversation_is_identical_with_or_without_attempt_tracking() {
        let agent = root(2);
        let mut plain = ObservationSnapshot::default();
        let request = request(&mut plain, &agent, None);
        let mut tracked = plain.clone();
        let attempt = 1;
        record(
            &mut tracked,
            &agent,
            SessionEvent::ModelAttemptStarted { request, attempt },
        );
        let call = ToolCall::new("call", "read", serde_json::json!({"path":"README.md"})).unwrap();
        let message = Message::Assistant(vec![
            AssistantItem::reasoning("reasoning", 0, "Normal reasoning", Some(replay())),
            AssistantItem::text("answer", 1, "Normal answer"),
            AssistantItem::tool_call("tool", 2, call),
        ]);
        for step in 0..3 {
            for snapshot in [&mut plain, &mut tracked] {
                match step {
                    0 => {
                        let activity = AgentActivity::Working;
                        let agent = agent.clone();
                        update(snapshot, RuntimeEvent::Activity { agent, activity });
                    }
                    1 => delta(snapshot, &agent, request, "text", "normal stream"),
                    _ => _ = commit(snapshot, &agent, message.clone()),
                }
            }
            for thinking in [false, true] {
                let normal = render_with(&plain, &agent, thinking, true);
                assert!(normal == render_with(&tracked, &agent, thinking, true));
                assert!(normal.iter().all(|entry| !entry.text().contains("attempt")));
            }
        }
    }

    #[test]
    fn retry_card_is_one_compact_safe_entry_and_leaves_the_stored_error_intact() {
        let agent = root(1);
        let mut snapshot = ObservationSnapshot::default();
        let request = request(&mut snapshot, &agent, None);
        let error = format!("HTTP 503 overloaded\n\u{1b}[31m{}", "x".repeat(1000));
        let failed = SessionEvent::ModelFailed {
            request,
            attempt: 1,
            error: error.clone(),
            kind: skyhook::session::ModelFailureKind::Error,
        };
        record(&mut snapshot, &agent, failed);
        let scheduled = SessionEvent::ModelRecoveryScheduled {
            request,
            attempt: 2,
            delay_millis: 1000,
            error: error.clone(),
        };
        record(&mut snapshot, &agent, scheduled);
        let cards = render(&snapshot, &agent, false);
        let text = cards[0].text();
        assert_eq!((cards.len(), text.lines().count()), (1, 2));
        assert!(text.contains("attempt 2") && text.contains("HTTP 503 overloaded"));
        assert!(!text.contains('\u{1b}') && text.chars().count() < 320 && text.ends_with('…'));
        assert!(snapshot.records.values().any(|r| matches!(&r.event,
            SessionEvent::ModelFailed { error: stored, .. } if stored == &error)));
    }
}

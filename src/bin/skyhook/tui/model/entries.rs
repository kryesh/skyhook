//! Tab selection and ordered conversation projection, including call/result provenance.
use crate::tui::app::OutputStore;

use super::jobs::{call_entry, job_entry};
use super::live::{block_key, reasoning_entry, response_entries, working_entry};
use super::notifications::job_event_entries;
use super::requests::request_entry;
use super::{
    Entry, EntryKey, EntryView, Projection, ResponseRef, Surface, Tab, Title, number, pretty,
};
use skyhook::agent::ObservationSnapshot;
use skyhook::identity::JobId;
use skyhook::provider::protocol::{AssistantItem, BlockRef};
use skyhook::session::{Message, MessageSeq, RecordSeq, RequestPhase, SessionEvent, UserPart};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolGroup {
    Response(MessageSeq),
    Script(JobId),
    Notification(RecordSeq, usize),
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
        tab,
        view,
        all_details,
    } = presentation;
    let records: Vec<_> = projection
        .records_by_agent
        .get(agent)
        .into_iter()
        .flatten()
        .filter_map(|sequence| snapshot.records.get(sequence))
        .collect();
    match tab {
        Tab::Requests => records
            .iter()
            .filter(|r| matches!(r.event, SessionEvent::ModelRequested { .. }))
            .filter_map(|r| {
                let record = projection.ledger.get(r.sequence.request())?;
                Some(request_entry(r.sequence.request(), record))
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
            let mut turn: Option<MessageSeq> = None;
            let mut call_results = HashMap::new();
            let mut matched_results = HashSet::new();
            for record in &records {
                match &record.event {
                    SessionEvent::MessageCommitted {
                        message: Message::Assistant(items),
                    } => {
                        pending.clear();
                        turn = Some(record.sequence.message());
                        for call in items.iter().filter_map(|item| item.call()) {
                            pending.insert(
                                (call.id().to_owned(), call.name().to_owned()),
                                record.sequence.message(),
                            );
                        }
                    }
                    SessionEvent::JobCreated {
                        origin: Some(origin),
                        tool,
                        ..
                    } if turn.is_none_or(|turn| turn <= origin.message) => {
                        // Retained job provenance also identifies a call whose
                        // assistant message is no longer in the retained history.
                        let message = origin.message;
                        pending.insert((origin.call_id.clone(), tool.clone()), message);
                        turn = Some(message);
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
                        let request = record.sequence.request();
                        if let Some(entry) =
                            super::retry::retry_entry(snapshot, projection, agent, request)
                        {
                            entries.push(entry);
                        }
                        // A settled response no commit replaced (an interrupted
                        // attempt) stays at its journal position; a failure's is
                        // part of its status card.
                        let phase = projection.ledger.get(request).map(|record| &record.phase);
                        if matches!(
                            phase,
                            Some(RequestPhase::Interrupted { .. } | RequestPhase::Completed { .. })
                        ) && let Some(response) =
                            snapshot.responses.get(&(agent.clone(), request))
                            && response.settlement().is_some()
                        {
                            entries.extend(response_entries(request, response, view, agent_name));
                        }
                    }
                    SessionEvent::MessageCommitted { message } => match message {
                        Message::User(blocks) => {
                            for (i, block) in blocks.iter().enumerate() {
                                let (title, text, surface) = match block {
                                    UserPart::Text { text } => (
                                        if agent.path().is_empty() {
                                            "You"
                                        } else {
                                            "Parent"
                                        },
                                        text.clone(),
                                        Surface::User,
                                    ),
                                    UserPart::ParentInput { text } => {
                                        ("Parent", text.clone(), Surface::User)
                                    }
                                    UserPart::Attachment { attachment } => {
                                        ("Attachment", pretty(attachment), Surface::User)
                                    }
                                    UserPart::JobEvents { events } => {
                                        for entry in job_event_entries(
                                            record.sequence,
                                            i,
                                            events,
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
                                    UserPart::State { .. } => continue,
                                    UserPart::Compaction { text } => {
                                        ("Compaction", text.clone(), Surface::Muted)
                                    }
                                };
                                entries.push(Entry::titled(
                                    EntryKey::UserBlock {
                                        record: record.sequence,
                                        index: i,
                                    },
                                    Title::plain(title),
                                    text,
                                    surface,
                                ));
                            }
                        }
                        Message::Assistant(items) => {
                            let message = record.sequence.message();
                            let response = projection
                                .ledger
                                .request_of(message)
                                .map_or(ResponseRef::Message(message), ResponseRef::Request);
                            let footer = projection
                                .ledger
                                .request_of(message)
                                .and_then(|request| projection.ledger.get(request))
                                .map(|request| request.profile.profile.model.clone());
                            // The model footer sits under the last visible text of an
                            // answer; a working turn (one with calls) has none.
                            let final_text = (!items.iter().any(|item| item.call().is_some()))
                                .then(|| {
                                    items.iter().rev().find_map(|item| match item {
                                        AssistantItem::Text { blocks, .. } => blocks
                                            .iter()
                                            .rfind(|block| !block.text.trim().is_empty()),
                                        _ => None,
                                    })
                                })
                                .flatten();
                            for item in items {
                                match item {
                                    AssistantItem::Text { id, blocks, .. } => {
                                        for block in blocks
                                            .iter()
                                            .filter(|block| !block.text.trim().is_empty())
                                        {
                                            let block_ref = BlockRef {
                                                item: id.clone(),
                                                block: block.id.clone(),
                                            };
                                            let mut entry = Entry::titled(
                                                block_key(response, &block_ref),
                                                Title::plain(agent_name),
                                                block.text.clone(),
                                                Surface::Agent,
                                            );
                                            if final_text
                                                .is_some_and(|last| std::ptr::eq(last, block))
                                            {
                                                entry.footer.clone_from(&footer);
                                            }
                                            entries.push(entry);
                                        }
                                    }
                                    AssistantItem::Reasoning { id, blocks, .. } => {
                                        for block in blocks
                                            .iter()
                                            .filter(|block| !block.text.trim().is_empty())
                                        {
                                            let block_ref = BlockRef {
                                                item: id.clone(),
                                                block: block.id.clone(),
                                            };
                                            entries.push(reasoning_entry(
                                                block_key(response, &block_ref),
                                                &block.text,
                                                view,
                                                super::live::ReasoningStatus::Complete,
                                            ));
                                        }
                                    }
                                    AssistantItem::ToolCall { call, .. } => {
                                        let exists = projection.tool_origins.contains(&(
                                            agent.clone(),
                                            message,
                                            call.id().to_owned(),
                                        ));
                                        if !exists {
                                            let call_key = EntryKey::ToolCall {
                                                message,
                                                call: call.id().to_owned(),
                                            };
                                            let e = call_entry(
                                                call_key.clone(),
                                                (
                                                    call.name(),
                                                    Some(call.arguments()),
                                                    call_results
                                                        .get(&(message, call.id().to_owned()))
                                                        .copied(),
                                                ),
                                                agent,
                                                projection,
                                                view.is_expanded(&call_key, all_details),
                                            );
                                            tool_groups.insert(
                                                e.key().clone(),
                                                ToolGroup::Response(message),
                                            );
                                            entries.push(e);
                                        }
                                    }
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
                        let open = view.is_expanded(&key, false);
                        let title = format!(
                            "Context compacted · {} → {}",
                            number(checkpoint.before_tokens),
                            number(checkpoint.after_tokens),
                        );
                        let body = if open {
                            format!("Summary and retained sources\n{}", pretty(checkpoint))
                        } else {
                            String::new()
                        };
                        let title = Title::disclosed(title, open);
                        entries.push(Entry::titled(key, title, body, Surface::Muted));
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
                response_entries(request, response, view, agent_name)
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
    use super::super::tests::{Journal, replay, update};
    use super::*;
    use skyhook::agent::{AgentActivity, RuntimeEvent};
    use skyhook::identity::AgentId;
    use skyhook::provider::protocol::{AssistantItem, ToolCall};

    fn render(snapshot: &ObservationSnapshot, agent: &AgentId, details: bool) -> Vec<Entry> {
        render_with(snapshot, agent, details)
    }

    fn render_with(snapshot: &ObservationSnapshot, agent: &AgentId, details: bool) -> Vec<Entry> {
        let mut projection = Projection::default();
        projection.rebuild(snapshot);
        let (view, outputs) = (View::default(), OutputStore::default());
        let view = EntryView {
            agent,
            tab: Tab::Conversation,
            view: &view,
            all_details: details,
        };
        entries(snapshot, &projection, view, &outputs, true)
    }

    async fn commit(journal: &mut Journal, agent: &AgentId, message: Message) -> MessageSeq {
        journal
            .record(agent, SessionEvent::MessageCommitted { message })
            .await
            .message()
    }

    fn user(text: &str) -> Message {
        Message::User(vec![UserPart::Text { text: text.into() }])
    }

    #[tokio::test]
    async fn synchronous_results_are_turn_scoped() {
        let mut journal = Journal::new().await;
        let agent = journal.agent();
        for error in [false, true] {
            journal.call_record(&agent, "reused").await;
            journal.result_record(&agent, "reused", error).await;
        }
        let cards = render(&journal.snapshot, &agent, true);
        assert_eq!(cards.len(), 2);
        assert!(cards[0].text().starts_with("▾ ✓ exec · Completed"));
        assert!(cards[1].text().starts_with("▾ × exec · Failed"));
        assert_ne!(cards[0].key(), cards[1].key());
        assert!(
            cards[1].text().contains("Output") && cards[1].text().contains("permission_denied")
        );
        assert!(cards.iter().all(|card| card.expandable()
            && card.job_id().is_none()
            && card.surface == Surface::Tool));

        // A new assistant turn closes the old call scope even if its old call
        // never produced a result: the late result stands alone.
        journal.call_record(&agent, "pending").await;
        journal.call_record(&agent, "new-turn").await;
        journal.result_record(&agent, "pending", true).await;
        let cards = render(&journal.snapshot, &agent, false);
        let texts: Vec<_> = cards.iter().map(Entry::text).collect();
        assert_eq!(texts[2..], ["▸ exec", "▸ exec", "▸ × exec · Failed"]);
    }

    #[tokio::test]
    async fn admitted_results_use_exact_job_provenance_even_without_retained_calls() {
        use skyhook::{execution::ExecutionLocation, session::ModelCallOrigin};
        for retained in [false, true] {
            let mut journal = Journal::new().await;
            let agent = journal.agent();
            let origin = journal.call_record(&agent, "reused").await;
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
                location: ExecutionLocation::root("/workspace".into()),
            };
            journal.record(&agent, created).await;
            journal.result_record(&agent, "reused", false).await;
            if !retained {
                journal.snapshot.records.remove(&origin.into());
            }
            // A later call reuses the ID but fails before creating a job.
            journal.call_record(&agent, "reused").await;
            journal.result_record(&agent, "reused", true).await;

            let cards = render(&journal.snapshot, &agent, false);
            assert_eq!(cards.len(), 2);
            assert_eq!(cards[0].job_id(), Some(JobId::new(42).unwrap()));
            assert_eq!(
                (cards[1].job_id(), cards[1].text()),
                (None, "▸ × exec · Failed")
            );
        }
    }

    #[tokio::test]
    async fn whitespace_only_turns_neither_create_agent_cards_nor_steal_the_answer_footer() {
        let call = ToolCall::new("call_read", "read", serde_json::json!({"path":"."})).unwrap();
        for whitespace in ["\n\n", "\n\n\n", " \t\r\n", "\u{2003}"] {
            let mut journal = Journal::new().await;
            let agent = journal.agent();
            journal.request(&agent, None).await;
            let reasoning = "Inspect the repository.";
            let message = Message::Assistant(vec![
                AssistantItem::reasoning("reasoning", 0, reasoning, Some(replay())),
                AssistantItem::text("separator", 1, whitespace),
                AssistantItem::tool_call("tool", 2, call.clone()),
            ]);
            commit(&mut journal, &agent, message).await;

            let cards = render(&journal.snapshot, &agent, true);
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

        let mut journal = Journal::new().await;
        let agent = journal.agent();
        journal.request(&agent, None).await;
        let answer = "  Actual answer with spacing.\n";
        let message = Message::Assistant(vec![
            AssistantItem::text("answer", 0, answer),
            AssistantItem::text("separator", 1, "\n\n"),
        ]);
        commit(&mut journal, &agent, message).await;
        let cards = render(&journal.snapshot, &agent, false);
        let answers: Vec<_> = cards
            .iter()
            .filter(|card| card.surface == Surface::Agent)
            .collect();
        assert_eq!(answers.len(), 1);
        assert!(answers[0].text().ends_with(answer));
        assert_eq!(answers[0].footer.as_deref(), Some("fixture-model"));
    }

    #[tokio::test]
    async fn refusals_reach_the_transcript_as_errors_through_the_real_projection() {
        // Guards the journal-to-card wiring: rendering a refusal as an ordinary
        // failure would restore the silent-failure UX this classification exists
        // to prevent.
        let mut journal = Journal::new().await;
        let agent = journal.agent();
        let refused_request = journal.request(&agent, None).await.request;
        let error = "content filter; the response contained no content".to_string();
        let refused = SessionEvent::ModelFailed {
            attempt: skyhook::session::AttemptRef {
                request: refused_request,
                attempt: 1,
            },
            error: error.clone(),
            kind: skyhook::session::ModelFailureKind::Refusal,
        };
        journal.record(&agent, refused).await;
        let entries = render(&journal.snapshot, &agent, false);
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
        let mut journal = Journal::new().await;
        let agent = journal.agent();
        let failed_request = journal.request(&agent, None).await.request;
        let failed = SessionEvent::ModelFailed {
            attempt: skyhook::session::AttemptRef {
                request: failed_request,
                attempt: 1,
            },
            error: "Protocol: rejected".to_owned(),
            kind: skyhook::session::ModelFailureKind::Error,
        };
        journal.record(&agent, failed).await;
        let entries = render(&journal.snapshot, &agent, false);
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

    #[tokio::test]
    async fn partial_attempts_stay_before_retry_and_followup_while_current_stream_stays_last() {
        let mut journal = Journal::new().await;
        let agent = journal.agent();
        let failed = journal.request(&agent, None).await;
        let context = Some(failed.context);
        let failed = failed.request;
        journal.delta(&agent, failed, "text", "failed partial");
        let error = "stream lost".into();
        let event = SessionEvent::ModelFailed {
            attempt: skyhook::session::AttemptRef {
                request: failed,
                attempt: 1,
            },
            error,
            kind: skyhook::session::ModelFailureKind::Error,
        };
        journal.record(&agent, event).await;
        let retry = journal.request(&agent, context).await.request;
        let answer = AssistantItem::text("text", 1, "successful retry");
        let message = commit(&mut journal, &agent, Message::Assistant(vec![answer])).await;
        journal
            .record(
                &agent,
                SessionEvent::ResponseCompleted {
                    attempt: skyhook::session::AttemptRef {
                        request: retry,
                        attempt: 1,
                    },
                    message,
                    outcome: skyhook::session::CompletedOutcome::Answer,
                },
            )
            .await;
        commit(&mut journal, &agent, user("new followup")).await;
        let interrupted = journal.request(&agent, context).await.request;
        journal.delta(&agent, interrupted, "reasoning", "interrupted thinking");
        journal.delta(&agent, interrupted, "text", "interrupted partial");
        let activity = AgentActivity::Stopped(skyhook::agent::TurnFailure::Interrupted);
        let event = RuntimeEvent::Activity {
            agent: agent.clone(),
            activity,
        };
        update(&mut journal.snapshot, event);
        let cut = skyhook::session::AttemptRef {
            request: interrupted,
            attempt: 1,
        };
        journal
            .record(&agent, SessionEvent::ModelAttemptInterrupted(cut))
            .await;
        let current = journal.request(&agent, context).await.request;
        journal.delta(&agent, current, "text", "current stream");
        journal.delta(&agent.child(1), retry, "text", "child only");

        let entries = render(&journal.snapshot, &agent, false);
        let text = entries
            .iter()
            .map(Entry::text)
            .collect::<Vec<_>>()
            .join("\n");
        let order = [
            "failed partial",
            "successful retry",
            "new followup",
            "Interrupted · attempt 1",
            "interrupted thinking",
            "Incomplete response\ninterrupted partial",
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
        assert_eq!(text.matches("interrupted partial").count(), 1);
        assert!(!text.contains("child only"));
        assert!(entries.last().unwrap().text().contains("current stream"));
    }

    #[tokio::test]
    async fn retry_card_is_one_compact_safe_entry_and_leaves_the_stored_error_intact() {
        let mut journal = Journal::new().await;
        let agent = journal.agent();
        let request = journal.request(&agent, None).await.request;
        let error = format!("HTTP 503 overloaded\n\u{1b}[31m{}", "x".repeat(1000));
        let failed = SessionEvent::ModelFailed {
            attempt: skyhook::session::AttemptRef {
                request,
                attempt: 1,
            },
            error: error.clone(),
            kind: skyhook::session::ModelFailureKind::Error,
        };
        let failure = journal.record(&agent, failed).await;
        let scheduled = SessionEvent::ModelRecoveryScheduled {
            failure,
            delay_millis: 1000,
        };
        journal.record(&agent, scheduled).await;
        let cards = render(&journal.snapshot, &agent, false);
        let text = cards[0].text();
        assert_eq!((cards.len(), text.lines().count()), (1, 2));
        assert!(text.contains("attempt 2") && text.contains("HTTP 503 overloaded"));
        assert!(!text.contains('\u{1b}') && text.chars().count() < 320 && text.ends_with('…'));
        assert!(journal.snapshot.records.values().any(|r| matches!(&r.event,
            SessionEvent::ModelFailed { error: stored, .. } if stored.as_str() == error)));
    }
}

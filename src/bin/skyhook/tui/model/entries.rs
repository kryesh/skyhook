//! Tab selection and ordered conversation projection, including call/result provenance.

use super::jobs::{call_entry, job_entry};
use super::live::{
    reasoning_entry, reasoning_key, response_block_key, response_entries, working_entry,
    working_label,
};
use super::notifications::{job_event_entries, job_notification_kind};
use super::requests::{request_entry, request_running};
use super::{Entry, EntryView, Projection, Surface, Tab, View, number, pretty};
use serde_json::Value;
use skyhook::agent::ObservationSnapshot;
use skyhook::identity::{AgentId, JobId};
use skyhook::provider::protocol::{BlockContent, Message, UserContent};
use skyhook::session::SessionEvent;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolGroup {
    Response(u64),
    Script(JobId),
    Notification(u64, usize),
}

pub fn entries(
    snapshot: &ObservationSnapshot,
    projection: &Projection,
    agent: &AgentId,
    view: &View,
    outputs: &HashMap<JobId, Value>,
    thinking: bool,
    all_details: bool,
) -> Vec<Entry> {
    entries_inner(
        snapshot,
        projection,
        EntryView {
            agent,
            view,
            thinking,
            all_details,
        },
        outputs,
        true,
    )
}

/// Shared by retained UI content and fresh export construction.
pub(super) fn entries_inner(
    snapshot: &ObservationSnapshot,
    projection: &Projection,
    presentation: EntryView<'_>,
    outputs: &HashMap<JobId, Value>,
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
            let recoveries: HashSet<_> = records
                .iter()
                .filter_map(|record| match &record.event {
                    SessionEvent::ModelRecoveryScheduled { request, .. } => Some(*request),
                    _ => None,
                })
                .collect();
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
                                pending
                                    .insert((call.id.clone(), call.name.clone()), record.sequence);
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
            let agent_name = projection
                .agents
                .iter()
                .find(|a| &a.id == agent)
                .map_or("Agent", |a| a.name.as_str());
            for record in &records {
                let key = format!("m{}", record.sequence);
                match &record.event {
                    SessionEvent::ModelRequested { .. } => {
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
                                    UserContent::Image { image } => (
                                        format!("Image attachment\n{}", pretty(image)),
                                        Surface::User,
                                    ),
                                    UserContent::Runtime { text }
                                        if job_notification_kind(text).is_some() =>
                                    {
                                        for entry in job_event_entries(
                                            &format!("{key}/{i}"),
                                            text,
                                            projection,
                                            view,
                                            all_details,
                                        ) {
                                            tool_groups.insert(
                                                entry.key.clone(),
                                                ToolGroup::Notification(record.sequence, i),
                                            );
                                            entries.push(entry);
                                        }
                                        continue;
                                    }
                                    UserContent::Runtime { text } => {
                                        (format!("Harness notification\n{text}"), Surface::Muted)
                                    }
                                    UserContent::Compaction { text } => {
                                        (format!("Compaction\n{text}"), Surface::Muted)
                                    }
                                };
                                entries.push(Entry::new(format!("{key}/{i}"), text, surface));
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
                                                .and_then(|request| request.model.clone());
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
                                            "Reasoning",
                                        ));
                                    }
                                    BlockContent::ToolCall(call) => {
                                        let exists = projection
                                            .tool_origins
                                            .contains(&(agent.clone(), record.sequence, call.id.clone()));
                                        if !exists {
                                            let e = call_entry(
                                                block_key.clone(), &call.name, Some(&call.arguments),
                                                call_results.get(&(record.sequence, call.id.clone())).copied(),
                                                agent, projection, view.is_expanded(&block_key, all_details),
                                            );
                                            tool_groups.insert(
                                                e.key.clone(),
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
                                    let result_key = format!("{key}/{}", result.call_id);
                                    let open = view.is_expanded(&result_key, all_details);
                                    entries.push(call_entry(
                                        result_key, &result.name, None,
                                        Some(result), agent, projection, open,
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
                                .filter(|parent| parent.tool == "script")
                                .map(|parent| ToolGroup::Script(parent.id))
                                .or_else(|| {
                                    origin
                                        .as_ref()
                                        .map(|origin| ToolGroup::Response(origin.message))
                                });
                            if let Some(group) = group {
                                tool_groups.insert(entry.key.clone(), group);
                            }
                            entries.push(entry);
                        }
                    }
                    SessionEvent::Compaction { checkpoint } => {
                        let key = format!("c{}", record.sequence);
                        let open = view.expanded.contains(&key);
                        let mut e = Entry::new(
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
                        e.expandable = true;
                        entries.push(e);
                    }
                    SessionEvent::Status { message } => entries.push(Entry::new(
                        key,
                        format!("Status · {message}"),
                        Surface::Status,
                    )),
                    SessionEvent::ModelFailed {
                        attempt,
                        error,
                        request,
                    } => {
                        if recoveries.contains(request) {
                            // The recovery status below represents this failure;
                            // keep partial output, but do not imply a terminal turn.
                            continue;
                        }
                        // This is the logical invocation count, not the provider's
                        // internal HTTP retry count. Do not promise a fixed /3 here.
                        // Exhausted HTTP retries report their count in the error.
                        let label = if *attempt == 1 {
                            "Request failed".to_owned()
                        } else {
                            format!("Request failed · attempt {attempt}")
                        };
                        entries.push(Entry::new(
                            format!("failed{request}"),
                            format!("{label}\n{error}"),
                            Surface::Error,
                        ));
                    }
                    SessionEvent::ModelRecoveryScheduled {
                        attempt,
                        max_attempts,
                        delay_millis,
                        error,
                        ..
                    } => entries.push(Entry::new(
                        key,
                        format!(
                            "Reconnecting · attempt {attempt} of {max_attempts} · retry delay {delay_millis} ms\n{error}"
                        ),
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
                let next_group = tool_groups.get(&entries[index + 1].key);
                let script_child = entries[index]
                    .job
                    .is_some_and(|job| next_group == Some(&ToolGroup::Script(job)));
                entries[index].compact_after = script_child
                    || tool_groups
                        .get(&entries[index].key)
                        .is_some_and(|group| next_group == Some(group));
            }
            if !include_live {
                return entries;
            }
            let mut responses: Vec<_> = snapshot
                .responses
                .iter()
                .filter(|((owner, request), response)| {
                    owner == agent && projection.live_response(*request, response)
                })
                .collect();
            responses.sort_by_key(|((_, request), _)| *request);
            entries.extend(responses.into_iter().flat_map(|((_, request), response)| {
                response_entries(*request, response, view, thinking, agent_name)
            }));
            if let Some(label) = working_label(
                snapshot,
                projection,
                agent,
                entries.iter().any(|entry| entry.running),
            ) {
                entries.push(working_entry(agent, &label));
            }
            entries
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use skyhook::agent::AgentActivity;
    use skyhook::agent::{ObservedEvent, RuntimeEvent};
    use skyhook::identity::SessionId;
    use skyhook::provider::protocol::ToolResult;
    use skyhook::provider::protocol::{
        AssistantItem, BlockKind, ContentDelta, ItemKind, ModelRequest, ReplayEnvelope,
        ResponseEvent,
    };
    use skyhook::session::ModelPurpose;
    use skyhook::session::{ContextMessage, EventRecord};

    fn update(snapshot: &mut ObservationSnapshot, event: RuntimeEvent) {
        snapshot.apply(ObservedEvent {
            revision: snapshot.revision + 1,
            event,
        });
    }

    fn stream(snapshot: &mut ObservationSnapshot, agent: &AgentId, request: u64, text: &str) {
        for event in [
            ResponseEvent::ItemStarted {
                id: "text".into(),
                position: 1,
                kind: ItemKind::Text,
            },
            ResponseEvent::BlockStarted {
                item: "text".into(),
                id: "text:0".into(),
                position: 0,
                kind: BlockKind::Text,
            },
            ResponseEvent::BlockDelta {
                item: "text".into(),
                block: "text:0".into(),
                delta: ContentDelta::Text(text.into()),
            },
        ] {
            update(
                snapshot,
                RuntimeEvent::ResponseEvent {
                    agent: agent.clone(),
                    request,
                    event,
                },
            );
        }
    }

    fn render(snapshot: &ObservationSnapshot, agent: &AgentId, details: bool) -> Vec<Entry> {
        let mut projection = Projection::default();
        projection.rebuild(snapshot);
        entries(
            snapshot,
            &projection,
            agent,
            &View::default(),
            &HashMap::new(),
            false,
            details,
        )
    }

    fn call_record(snapshot: &mut ObservationSnapshot, agent: &AgentId, id: &str) -> u64 {
        record(
            snapshot,
            agent,
            SessionEvent::MessageCommitted {
                message: Message::Assistant(vec![AssistantItem::tool_call(
                    "tool",
                    0,
                    skyhook::provider::protocol::ToolCall {
                        id: id.into(),
                        name: "exec".into(),
                        arguments: serde_json::json!({"argv": ["echo", "  original\ttext\n"]}),
                    },
                )]),
            },
        )
    }

    fn context(snapshot: &mut ObservationSnapshot, agent: &AgentId) -> u64 {
        record(
            snapshot,
            agent,
            SessionEvent::ModelContext {
                provider: "fixture".into(),
                template: ModelRequest {
                    model: "fixture-model".into(),
                    system: vec![],
                    messages: vec![],
                    tools: vec![],
                    response_schema: None,
                    reasoning: None,
                    max_output_tokens: Some(100),
                    correlation: None,
                },
            },
        )
    }

    fn record(snapshot: &mut ObservationSnapshot, agent: &AgentId, event: SessionEvent) -> u64 {
        let sequence = snapshot
            .records
            .last_key_value()
            .map_or(1, |(sequence, _)| sequence + 1);
        snapshot.apply(ObservedEvent {
            revision: snapshot.revision + 1,
            event: RuntimeEvent::Record(Box::new(EventRecord {
                version: 1,
                sequence,
                timestamp_millis: 0,
                agent: agent.clone(),
                event,
            })),
        });
        sequence
    }

    fn replay() -> ReplayEnvelope {
        ReplayEnvelope {
            version: 1,
            protocol: "fixture".into(),
            model: "fixture".into(),
            scope: "reasoning".into(),
            payload: serde_json::json!({"signature": "opaque"}),
        }
    }

    fn request(snapshot: &mut ObservationSnapshot, agent: &AgentId, context: u64) -> u64 {
        record(
            snapshot,
            agent,
            SessionEvent::ModelRequested {
                context,
                messages: vec![ContextMessage::Inline {
                    message: Message::User(vec![UserContent::Text {
                        text: "original request".into(),
                    }]),
                }],
                purpose: ModelPurpose::Agent,
            },
        )
    }

    fn result_record(snapshot: &mut ObservationSnapshot, agent: &AgentId, id: &str, error: bool) {
        record(
            snapshot,
            agent,
            SessionEvent::MessageCommitted {
                message: Message::Tool(vec![ToolResult {
                    call_id: id.into(),
                    name: "exec".into(),
                    result: if error {
                        serde_json::json!({"error": "Permission was denied", "code": "permission_denied", "executed": false})
                    } else {
                        serde_json::json!({"stdout": "  original\ttext\n"})
                    },
                    images: vec![],
                    is_error: error,
                }]),
            },
        );
    }

    #[test]
    fn synchronous_results_are_turn_scoped_and_orphans_remain_expandable() {
        let root = AgentId::root(SessionId::from_bytes([72; 16]));
        let mut snapshot = ObservationSnapshot::default();
        call_record(&mut snapshot, &root, "reused");
        result_record(&mut snapshot, &root, "reused", false);
        call_record(&mut snapshot, &root, "reused");
        result_record(&mut snapshot, &root, "reused", true);
        result_record(&mut snapshot, &root, "orphan", true);

        let cards = render(&snapshot, &root, true);
        assert_eq!(cards.len(), 3);
        assert!(cards[0].text.starts_with("▾ ✓ exec · Completed"));
        assert!(cards[1].text.starts_with("▾ × exec · Failed"));
        assert_ne!(cards[0].key, cards[1].key);
        assert!(cards[2].text.starts_with("▾ × exec · Failed"));
        assert!(!cards[2].text.contains("Arguments"));
        assert!(cards[2].text.contains("Output"));
        assert!(cards[2].text.contains("permission_denied"));
        assert!(
            cards
                .iter()
                .all(|card| card.expandable && card.job.is_none() && card.surface == Surface::Tool)
        );

        // A new assistant turn closes the old call scope even if its old call
        // never produced a result. Same-ID results in other agents cannot bind.
        let other = root.child(1);
        result_record(&mut snapshot, &other, "reused", true);
        call_record(&mut snapshot, &root, "pending");
        call_record(&mut snapshot, &root, "new-turn");
        result_record(&mut snapshot, &root, "pending", true);

        let cards = render(&snapshot, &root, false);
        assert_eq!(cards.len(), 6);
        assert_eq!(cards[3].text, "▸ exec");
        assert_eq!(cards[4].text, "▸ exec");
        assert_eq!(cards[5].text, "▸ × exec · Failed");
        let other_cards = render(&snapshot, &other, false);
        assert_eq!(other_cards.len(), 1);
    }

    #[test]
    fn admitted_results_use_exact_job_provenance_even_without_retained_calls() {
        use skyhook::{execution::ExecutionLocation, session::ModelCallOrigin};
        for retained in [false, true] {
            let root = AgentId::root(SessionId::from_bytes([73; 16]));
            let mut snapshot = ObservationSnapshot::default();
            let origin = call_record(&mut snapshot, &root, "reused");
            record(
                &mut snapshot,
                &root,
                SessionEvent::JobCreated {
                    job: JobId::new(42).unwrap(),
                    parent: None,
                    origin: Some(ModelCallOrigin {
                        message: origin,
                        call_id: "reused".into(),
                    }),
                    tool: "exec".into(),
                    name: None,
                    arguments: serde_json::json!({"argv": ["echo"]}),
                    output_schema: None,
                    accepts_input: false,
                    background: false,
                    location: ExecutionLocation::root("/workspace".into()),
                },
            );
            result_record(&mut snapshot, &root, "reused", false);
            if !retained {
                snapshot.records.remove(&origin);
            }
            // A later call reuses the ID but fails before creating a job.
            call_record(&mut snapshot, &root, "reused");
            result_record(&mut snapshot, &root, "reused", true);

            let cards = render(&snapshot, &root, false);
            assert_eq!(cards.len(), 2);
            assert_eq!(cards[0].job, Some(JobId::new(42).unwrap()));
            assert!(cards[1].job.is_none());
            assert_eq!(cards[1].text, "▸ × exec · Failed");
        }
    }

    #[test]
    fn whitespace_only_tool_turns_do_not_create_empty_agent_cards() {
        for whitespace in ["\n\n", "\n\n\n", " \t\r\n", "\u{2003}"] {
            let mut snapshot = ObservationSnapshot::default();
            let root = AgentId::root(SessionId::from_bytes([3; 16]));
            let context = context(&mut snapshot, &root);
            request(&mut snapshot, &root, context);
            let message = Message::Assistant(vec![
                AssistantItem::reasoning("reasoning", 0, "Inspect the repository.", Some(replay())),
                AssistantItem::text("separator", 1, whitespace),
                AssistantItem::tool_call(
                    "tool",
                    2,
                    skyhook::provider::protocol::ToolCall {
                        id: "call_read".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path":"."}),
                    },
                ),
            ]);
            record(
                &mut snapshot,
                &root,
                SessionEvent::MessageCommitted { message },
            );

            let cards = render(&snapshot, &root, true);
            assert!(cards.iter().all(|card| card.surface != Surface::Agent));
            assert!(cards.iter().any(|card| card.surface == Surface::Reasoning
                && card.text.contains("Inspect the repository.")));
            assert!(
                cards
                    .iter()
                    .any(|card| card.surface == Surface::Tool && card.text.contains("read"))
            );
        }
    }

    #[test]
    fn trailing_whitespace_does_not_steal_the_final_answer_footer() {
        let mut snapshot = ObservationSnapshot::default();
        let root = AgentId::root(SessionId::from_bytes([5; 16]));
        let context = context(&mut snapshot, &root);
        request(&mut snapshot, &root, context);
        let answer = "  Actual answer with spacing.\n";
        record(
            &mut snapshot,
            &root,
            SessionEvent::MessageCommitted {
                message: Message::Assistant(vec![
                    AssistantItem::text("answer", 0, answer),
                    AssistantItem::text("separator", 1, "\n\n"),
                ]),
            },
        );

        let cards = render(&snapshot, &root, false);
        let answers: Vec<_> = cards
            .iter()
            .filter(|card| card.surface == Surface::Agent)
            .collect();
        assert_eq!(answers.len(), 1);
        assert!(answers[0].text.ends_with(answer));
        assert_eq!(answers[0].footer.as_deref(), Some("fixture-model"));
    }

    #[test]
    fn conversation_projects_mixed_and_legacy_job_events_without_reclassifying_user_text() {
        let agent = AgentId::root(SessionId::from_bytes([2; 16]));
        let messages = format!(
            "<skyhook_agent_messages>\n{}\n</skyhook_agent_messages>",
            serde_json::json!([
                {"id":253,"message":6577,"text":"first progress"},
                {"id":253,"message":6578,"text":"second progress"},
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
        let restored = serde_json::from_slice(&saved).unwrap();
        let mut snapshot = ObservationSnapshot::default();
        let sequence = record(
            &mut snapshot,
            &agent,
            SessionEvent::MessageCommitted { message: restored },
        );

        let cards = render(&snapshot, &agent, true);
        let notifications: Vec<_> = cards
            .iter()
            .filter(|entry| entry.surface == Surface::Tool)
            .collect();
        assert_eq!(notifications.len(), 6);
        assert_eq!(notifications[0].key, format!("m{sequence}/0/event0"));
        assert_eq!(notifications[1].key, format!("m{sequence}/0/event1"));
        for (index, notification) in notifications[2..].iter().enumerate() {
            assert_eq!(notification.key, format!("m{sequence}/1/event{index}"));
        }
        assert!(notifications[0].text.contains("first progress"));
        assert!(notifications[1].text.contains("second progress"));
        assert!(
            notifications[2]
                .text
                .contains("agent #253 · reviewer · message #6579")
        );
        assert!(
            notifications[2]
                .text
                .contains("Agent message received by model")
        );
        assert!(notifications[2].text.contains("independent reply"));
        assert!(notifications[3].text.contains("agent #253 · completed"));
        assert!(
            !notifications[3]
                .text
                .contains("Agent message received by model")
        );
        assert!(!notifications[3].text.contains("independent reply"));
        assert!(notifications[4].text.contains("agent #254 · message #6580"));
        assert!(
            notifications[4]
                .text
                .contains("Agent message received by model")
        );
        assert!(notifications[4].text.contains("another child reply"));
        assert!(notifications[5].text.contains("exec #255 · completed"));
        assert!(notifications[5].text.contains("tool output"));
        assert!(
            !notifications[5]
                .text
                .contains("Agent message received by model")
        );
        assert!(notifications.iter().all(|entry| entry.expandable
            && entry.job.is_none()
            && !entry.text.contains("<skyhook_")
            && !entry.text.contains("Harness notification")));
        assert!(notifications[0].compact_after);
        assert!(!notifications[1].compact_after);
        assert!(cards.iter().any(
            |entry| entry.surface == Surface::User && entry.text == format!("You\n{messages}")
        ));
        assert!(cards.iter().any(|entry| entry.surface == Surface::Muted
            && entry.text == "Harness notification\nordinary scheduler note"));
        let SessionEvent::MessageCommitted { message: stored } = &snapshot.records[&sequence].event
        else {
            panic!("message history changed during rendering");
        };
        assert_eq!(serde_json::to_vec(stored).unwrap(), saved);
    }

    #[test]
    fn failed_request_labels_do_not_confuse_http_retries_with_invocations() {
        for (attempt, error, label) in [
            (
                1,
                "Timeout: provider HTTP startup timeout (after 3 HTTP attempts)",
                "Request failed\n",
            ),
            (2, "Protocol: rejected", "Request failed · attempt 2\n"),
        ] {
            let mut snapshot = ObservationSnapshot::default();
            let root = AgentId::root(SessionId::from_bytes([1; 16]));
            let context = context(&mut snapshot, &root);
            let request = request(&mut snapshot, &root, context);
            record(
                &mut snapshot,
                &root,
                SessionEvent::ModelFailed {
                    request,
                    attempt,
                    error: error.into(),
                },
            );

            let entries = render(&snapshot, &root, false);
            let failure = entries
                .iter()
                .find(|entry| entry.key == format!("failed{request}"))
                .unwrap();
            assert_eq!(failure.text, format!("{label}{error}"));
            assert!(!failure.text.contains("/3"));
        }
    }

    #[test]
    fn partial_attempts_stay_before_retry_and_followup_while_current_stream_stays_last() {
        let mut snapshot = ObservationSnapshot::default();
        let root = AgentId::root(SessionId::from_bytes([1; 16]));
        let child = root.child(1);
        let context = context(&mut snapshot, &root);
        let failed = request(&mut snapshot, &root, context);
        stream(&mut snapshot, &root, failed, "failed partial");
        record(
            &mut snapshot,
            &root,
            SessionEvent::ModelFailed {
                request: failed,
                attempt: 1,
                error: "stream lost".into(),
            },
        );
        let retry = request(&mut snapshot, &root, context);
        record(
            &mut snapshot,
            &root,
            SessionEvent::MessageCommitted {
                message: Message::Assistant(vec![AssistantItem::text(
                    "text",
                    1,
                    "successful retry",
                )]),
            },
        );
        record(
            &mut snapshot,
            &root,
            SessionEvent::MessageCommitted {
                message: Message::User(vec![UserContent::Text {
                    text: "new followup".into(),
                }]),
            },
        );
        let interrupted = request(&mut snapshot, &root, context);
        stream(&mut snapshot, &root, interrupted, "interrupted partial");
        update(
            &mut snapshot,
            RuntimeEvent::Activity {
                agent: root.clone(),
                activity: AgentActivity::Interrupted,
            },
        );
        let current = request(&mut snapshot, &root, context);
        stream(&mut snapshot, &root, current, "current stream");
        stream(&mut snapshot, &child, retry, "child only");

        let entries = render(&snapshot, &root, false);
        let text = entries
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for (a, b) in [
            ("failed partial", "successful retry"),
            ("successful retry", "new followup"),
            ("new followup", "interrupted partial"),
            ("interrupted partial", "current stream"),
        ] {
            assert!(
                text.find(a).unwrap() < text.find(b).unwrap(),
                "{a} must precede {b}: {text}"
            );
        }
        assert_eq!(text.matches("failed partial").count(), 1);
        assert!(!text.contains("child only"));
        assert!(entries.last().unwrap().text.contains("current stream"));
    }
}

use super::entries::entries_inner;
use super::jobs::job_entry;
use super::live::{response_entries, working_entry, working_label};
use super::requests::{request_elapsed, request_entry, request_running};
use super::{Entry, EntryView, Projection, Tab};
use serde_json::Value;
use skyhook::agent::ObservationSnapshot;
use skyhook::identity::{AgentId, JobId};
use skyhook::session::SessionEvent;
use std::collections::{HashMap, HashSet};

/// Indices refer to the caller-owned entry vector. `appends` maps a subset of
/// `dirty` to their previous text byte lengths; their prefixes are unchanged.
#[derive(Default, Debug)]
pub struct ContentChanges {
    pub dirty: Vec<usize>,
    pub appends: HashMap<usize, usize>,
    pub reset: bool,
}

/// Retains rendered journal content across streaming events. The caller bumps
/// `revision` for history and presentation changes. ResponseEvent mutations use
/// `observe_response` so authoritative replacements rebuild only the live tail.
/// Journal progress, selected agent, tab and defaults are also checked here.
///
/// Entries remain caller-owned: neither historical strings nor tool documents
/// are cloned to hand the cache's result to the renderer.
#[derive(Default)]
pub struct ContentCache {
    identity: Option<(AgentId, Tab, bool, bool, u64, u64)>,
    history_len: usize,
    history_running: bool,
    live: Vec<LiveContent>,
    dirty_responses: HashMap<AgentId, HashSet<u64>>,
    job_indices: HashMap<JobId, usize>,
    invalid_jobs: HashSet<JobId>,
}

struct LiveContent {
    request: u64,
    start: usize,
    count: usize,
}

impl ContentCache {
    /// Response events can replace content, reorder blocks, or end a block
    /// without changing its text. Never infer cache validity from string lengths.
    pub fn observe_response(&mut self, agent: &AgentId, request: u64) {
        self.dirty_responses
            .entry(agent.clone())
            .or_default()
            .insert(request);
    }

    /// Refresh downloaded output for one tool without invalidating history.
    pub fn invalidate_job(&mut self, job: JobId) {
        self.invalid_jobs.insert(job);
    }

    fn finish_reset(
        &mut self,
        entries: &mut [Entry],
        old: Vec<Entry>,
        changes: &mut ContentChanges,
    ) {
        self.job_indices.clear();
        for (index, entry) in entries.iter().enumerate() {
            if let Some(job) = entry.job {
                self.job_indices.insert(job, index);
            }
        }
        self.invalid_jobs.clear();
        // Preserve warm layout for journal/output changes that leave ordering
        // intact, including response snapshot replacements. Reuse equal
        // allocations and retain layout for unchanged entries.
        changes.reset =
            old.is_empty() || old.iter().zip(entries.iter()).any(|(a, b)| a.key != b.key);
        changes.dirty.clear();
        changes.appends.clear();
        if !changes.reset {
            let old_len = old.len();
            for (index, previous) in old.into_iter().enumerate().take(entries.len()) {
                if previous == entries[index] {
                    entries[index] = previous;
                } else {
                    changes.dirty.push(index);
                }
            }
            changes.dirty.extend(old_len..entries.len());
        }
    }

    pub fn update(
        &mut self,
        entries: &mut Vec<Entry>,
        snapshot: &ObservationSnapshot,
        projection: &Projection,
        presentation: EntryView<'_>,
        outputs: &HashMap<JobId, Value>,
        revision: u64,
    ) -> ContentChanges {
        let EntryView {
            agent,
            view,
            thinking,
            all_details,
        } = presentation;
        let identity = (
            agent.clone(),
            view.tab,
            thinking,
            all_details,
            revision,
            projection.through,
        );
        let reset = self.identity.as_ref() != Some(&identity);
        let dirty_responses = self.dirty_responses.remove(agent).unwrap_or_default();
        let mut changes = ContentChanges {
            reset,
            ..ContentChanges::default()
        };
        let old = if reset {
            Some(std::mem::take(entries))
        } else {
            None
        };
        let agent_name = projection
            .agents
            .iter()
            .find(|a| &a.id == agent)
            .map_or("Agent", |a| a.name.as_str());
        if reset {
            self.identity = Some(identity);
            *entries = entries_inner(snapshot, projection, presentation, outputs, false);
            self.history_len = entries.len();
            self.history_running = entries.iter().any(|entry| entry.running);
            self.live.clear();
        }
        if !reset {
            for job in self.invalid_jobs.drain() {
                if let Some(&index) = self.job_indices.get(&job)
                    && let Some(info) = projection.jobs.get(&job)
                {
                    let compact_after = entries[index].compact_after;
                    entries[index] = job_entry(info, projection, view, outputs, all_details);
                    entries[index].compact_after = compact_after;
                    changes.dirty.push(index);
                }
            }
        }
        if view.tab == Tab::Requests && !reset {
            // Elapsed time changes without a new journal record.
            if let Some(&request) = projection.active_request.get(agent)
                && let Some(info) = projection.requests.get(&request)
                && let Some(index) = entries
                    .iter()
                    .position(|entry| entry.key == format!("r{request}"))
            {
                let running = request_running(info, snapshot, projection, agent, request);
                let entry = &mut entries[index];
                let elapsed_tenths = request_elapsed(info, running);
                if entry.running != running {
                    if let Some(record) = snapshot.records.get(&request)
                        && let SessionEvent::ModelRequested { purpose, .. } = &record.event
                    {
                        *entry = request_entry(request, purpose, info, running);
                        changes.dirty.push(index);
                    }
                } else if let Some(row) = &mut entry.request
                    && row.elapsed_tenths != elapsed_tenths
                {
                    row.elapsed_tenths = elapsed_tenths;
                    changes.dirty.push(index);
                }
            }
        }
        if view.tab != Tab::Conversation {
            if let Some(old) = old {
                self.finish_reset(entries, old, &mut changes);
            }
            return changes;
        }
        if reset {
            let mut responses: Vec<_> = snapshot
                .responses
                .iter()
                .filter(|((owner, request), response)| {
                    owner == agent && projection.live_response(*request, response)
                })
                .collect();
            responses.sort_by_key(|((_, request), _)| *request);
            for ((_, request), response) in responses {
                let start = entries.len();
                entries.extend(response_entries(
                    *request, response, view, thinking, agent_name,
                ));
                self.live.push(LiveContent {
                    request: *request,
                    start,
                    count: entries.len() - start,
                });
            }
        }
        if !reset && !dirty_responses.is_empty() {
            // Native events can replace equal-length text, reorder items, or end
            // blocks. Rebuild only the live suffix, never clone journal entries.
            let previous = entries.split_off(self.history_len);
            let mut requests: Vec<_> = self
                .live
                .iter()
                .map(|live| live.request)
                .chain(dirty_responses.iter().copied())
                .collect();
            requests.sort_unstable();
            requests.dedup();
            self.live.clear();
            for request in requests {
                if let Some(response) = snapshot.responses.get(&(agent.clone(), request))
                    && projection.live_response(request, response)
                {
                    let start = entries.len();
                    entries.extend(response_entries(
                        request, response, view, thinking, agent_name,
                    ));
                    self.live.push(LiveContent {
                        request,
                        start,
                        count: entries.len() - start,
                    });
                }
            }
            // Keep unchanged live allocations, too. The old working indicator
            // is regenerated below; entry removal is conveyed by vector length.
            let reordered = previous
                .iter()
                .zip(&entries[self.history_len..])
                .any(|(old, new)| old.key != new.key);
            changes.reset |= reordered;
            let previous_len = previous.len();
            for (offset, old) in previous.into_iter().enumerate() {
                let index = self.history_len + offset;
                if let Some(entry) = entries.get_mut(index) {
                    if *entry == old {
                        *entry = old;
                    } else {
                        changes.dirty.push(index);
                    }
                }
            }
            changes
                .dirty
                .extend(self.history_len + previous_len..entries.len());
        }
        // The working indicator is a synthetic tail, never part of history.
        let end = self
            .live
            .last()
            .map_or(self.history_len, |l| l.start + l.count);
        let working_key = format!("working-{agent}");
        let running = self.history_running
            || entries[self.history_len..end]
                .iter()
                .any(|entry| entry.running);
        let has_working = entries.get(end).is_some_and(|e| e.key == working_key);
        if let Some(label) = working_label(snapshot, projection, agent, running) {
            let entry = working_entry(agent, &label);
            if !has_working {
                entries.insert(end, entry);
                changes.dirty.extend(end..entries.len());
            } else if entries[end] != entry {
                entries[end] = entry;
                changes.dirty.push(end);
            }
        } else if has_working {
            entries.remove(end);
            changes.dirty.extend(end..=entries.len());
        }
        if let Some(old) = old {
            self.finish_reset(entries, old, &mut changes);
        }
        changes.dirty.sort_unstable();
        changes.dirty.dedup();
        changes.appends.retain(|index, _| *index < entries.len());
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::super::entries;
    use super::super::{JobInfo, Surface, View};
    use super::*;
    use skyhook::agent::AgentActivity;
    use skyhook::agent::{ObservedEvent, RuntimeEvent};
    use skyhook::identity::SessionId;
    use skyhook::job::JobState;
    use skyhook::provider::protocol::{
        AssistantItem, BlockContent, BlockKind, ContentDelta, ItemKind, ModelRequest,
        ReplayEnvelope, ResponseEvent,
    };
    use skyhook::provider::protocol::{Message, ToolResult, UserContent};
    use skyhook::session::ModelPurpose;
    use skyhook::session::{ContextMessage, EventRecord};

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

    fn delta_event(agent: AgentId, request: u64, kind: BlockKind, text: String) -> RuntimeEvent {
        let item = match kind {
            BlockKind::Text => "text",
            BlockKind::Reasoning => "reasoning",
            _ => unreachable!(),
        };
        RuntimeEvent::ResponseEvent {
            agent,
            request,
            event: ResponseEvent::BlockDelta {
                item: item.into(),
                block: format!("{item}:0"),
                delta: ContentDelta::Text(text),
            },
        }
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

    fn response_event(
        snapshot: &mut ObservationSnapshot,
        agent: &AgentId,
        request: u64,
        event: ResponseEvent,
    ) {
        update(
            snapshot,
            RuntimeEvent::ResponseEvent {
                agent: agent.clone(),
                request,
                event,
            },
        );
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

    fn update(snapshot: &mut ObservationSnapshot, event: RuntimeEvent) {
        // Delta fixtures start their native item/block once; production receives only full protocol events.
        if let RuntimeEvent::ResponseEvent {
            agent,
            request,
            event: ResponseEvent::BlockDelta { item, block, .. },
        } = &event
        {
            let exists = snapshot
                .responses
                .get(&(agent.clone(), *request))
                .is_some_and(|live| live.snapshot().items.iter().any(|entry| entry.id == *item));
            if !exists {
                let (position, kind, block_kind) = if item == "reasoning" {
                    (0, ItemKind::Reasoning, BlockKind::Reasoning)
                } else {
                    (1, ItemKind::Text, BlockKind::Text)
                };
                response_event(
                    snapshot,
                    agent,
                    *request,
                    ResponseEvent::ItemStarted {
                        id: item.clone(),
                        position,
                        kind,
                    },
                );
                response_event(
                    snapshot,
                    agent,
                    *request,
                    ResponseEvent::BlockStarted {
                        item: item.clone(),
                        id: block.clone(),
                        position: 0,
                        kind: block_kind,
                    },
                );
            }
        }
        snapshot.apply(ObservedEvent {
            revision: snapshot.revision + 1,
            event,
        });
    }

    #[test]
    fn synchronous_failure_refreshes_the_existing_call_after_interleaved_user_input() {
        let root = AgentId::root(SessionId::from_bytes([71; 16]));
        let mut snapshot = ObservationSnapshot::default();
        call_record(&mut snapshot, &root, "call");
        let mut projection = Projection::default();
        projection.rebuild(&snapshot);
        let mut cache = ContentCache::default();
        let mut cards = vec![];
        let outputs = HashMap::new();
        let mut view = View::default();
        cache.update(
            &mut cards,
            &snapshot,
            &projection,
            EntryView {
                agent: &root,
                view: &view,
                thinking: false,
                all_details: false,
            },
            &outputs,
            0,
        );
        assert_eq!(cards.len(), 1);
        let key = cards[0].key.clone();
        assert!(!cards[0].text.contains("Failed"));
        record(
            &mut snapshot,
            &root,
            SessionEvent::MessageCommitted {
                message: Message::User(vec![UserContent::Text {
                    text: "Continue after permission".into(),
                }]),
            },
        );
        result_record(&mut snapshot, &root, "call", true);
        let records_before = serde_json::to_value(&snapshot.records).unwrap();
        projection.rebuild(&snapshot);
        let changes = cache.update(
            &mut cards,
            &snapshot,
            &projection,
            EntryView {
                agent: &root,
                view: &view,
                thinking: false,
                all_details: false,
            },
            &outputs,
            0,
        );
        assert!(!changes.reset);
        assert!(changes.dirty.contains(&0));
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].key, key);
        assert_eq!(cards[0].surface, Surface::Tool);
        assert!(cards[0].job.is_none());
        assert_eq!(cards[0].text, "▸ × exec · Failed");
        assert!(!cards[0].text.contains("Permission"));
        view.expanded.insert(key.clone());
        cache.update(
            &mut cards,
            &snapshot,
            &projection,
            EntryView {
                agent: &root,
                view: &view,
                thinking: false,
                all_details: false,
            },
            &outputs,
            1,
        );
        assert_eq!(cards[0].key, key);
        assert!(cards[0].text.contains("Arguments"));
        assert!(cards[0].text.contains("Output\n  Permission was denied"));
        assert!(cards[0].text.contains("permission_denied"));
        assert_eq!(cards[0].text.matches("Permission was denied").count(), 1);
        assert_eq!(
            records_before,
            serde_json::to_value(&snapshot.records).unwrap()
        );
    }

    #[test]
    fn content_cache_refreshes_only_invalidated_tool_output() {
        let agent = AgentId::root(SessionId::from_bytes([34; 16]));
        let snapshot = ObservationSnapshot::default();
        let mut projection = Projection::default();
        for id in 1..=2 {
            let id = JobId::new(id).unwrap();
            projection.jobs.insert(
                id,
                JobInfo {
                    id,
                    agent: agent.clone(),
                    name: None,
                    tool: "exec".into(),
                    args: serde_json::json!({"argv": ["echo", "hi"]}),
                    parent: None,
                    state: JobState::Completed,
                    target: "root".into(),
                    location: "/tmp".into(),
                    remote: false,
                    error: None,
                },
            );
        }
        let view = View {
            tab: Tab::Jobs,
            ..View::default()
        };
        let mut outputs = HashMap::new();
        let mut cache = ContentCache::default();
        let mut rows = Vec::new();
        cache.update(
            &mut rows,
            &snapshot,
            &projection,
            EntryView {
                agent: &agent,
                view: &view,
                thinking: false,
                all_details: true,
            },
            &outputs,
            0,
        );
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|entry| entry.compact_after));
        let unchanged = rows[1].clone();
        let job = JobId::new(1).unwrap();
        outputs.insert(
            job,
            serde_json::json!({"stdout": "new output", "exit_code": 0}),
        );
        cache.invalidate_job(job);
        let changes = cache.update(
            &mut rows,
            &snapshot,
            &projection,
            EntryView {
                agent: &agent,
                view: &view,
                thinking: false,
                all_details: true,
            },
            &outputs,
            0,
        );
        assert!(!changes.reset);
        assert_eq!(changes.dirty, vec![0]);
        assert!(rows[0].text.contains("new output"));
        assert!(rows.iter().all(|entry| entry.compact_after));
        assert!(rows[1] == unchanged);
        assert!(rows == entries(&snapshot, &projection, &agent, &view, &outputs, false, true,));
    }

    #[test]
    fn content_cache_reasoning_matches_uncached_across_shape_changes() {
        let agent = AgentId::root(SessionId::from_bytes([32; 16]));
        let mut snapshot = ObservationSnapshot::default();
        let context = context(&mut snapshot, &agent);
        let request = request(&mut snapshot, &agent, context);
        let mut projection = Projection::default();
        projection.rebuild(&snapshot);
        let view = View::default();
        let outputs = HashMap::new();
        let mut cache = ContentCache::default();
        let mut rows = Vec::new();
        cache.observe_response(&agent, request);
        cache.update(
            &mut rows,
            &snapshot,
            &projection,
            EntryView {
                agent: &agent,
                view: &view,
                thinking: false,
                all_details: false,
            },
            &outputs,
            0,
        );
        for text in ["\n", "first", "\n", "second", "\r\n", "third", "\n\n"] {
            update(
                &mut snapshot,
                delta_event(agent.clone(), request, BlockKind::Reasoning, text.into()),
            );
            cache.observe_response(&agent, request);
            cache.update(
                &mut rows,
                &snapshot,
                &projection,
                EntryView {
                    agent: &agent,
                    view: &view,
                    thinking: false,
                    all_details: false,
                },
                &outputs,
                0,
            );
            let expected = entries(
                &snapshot,
                &projection,
                &agent,
                &view,
                &outputs,
                false,
                false,
            );
            assert!(rows == expected);
        }
        update(
            &mut snapshot,
            delta_event(agent.clone(), request, BlockKind::Text, "answer".into()),
        );
        cache.observe_response(&agent, request);
        cache.update(
            &mut rows,
            &snapshot,
            &projection,
            EntryView {
                agent: &agent,
                view: &view,
                thinking: false,
                all_details: false,
            },
            &outputs,
            0,
        );
        let expected = entries(
            &snapshot,
            &projection,
            &agent,
            &view,
            &outputs,
            false,
            false,
        );
        assert!(rows == expected);
        // Equal-length authoritative replacement and block-only closure must invalidate cards.
        let provisional = snapshot.responses[&(agent.clone(), request)]
            .snapshot()
            .items[0]
            .blocks[0]
            .text
            .clone();
        let replacement = provisional.replace("first", "FIRST");
        assert_eq!(replacement.len(), provisional.len());
        response_event(
            &mut snapshot,
            &agent,
            request,
            ResponseEvent::BlockEnded {
                item: "reasoning".into(),
                block: "reasoning:0".into(),
                content: BlockContent::Reasoning { text: replacement },
            },
        );
        response_event(
            &mut snapshot,
            &agent,
            request,
            ResponseEvent::ItemEnded {
                id: "reasoning".into(),
                replay: Some(replay()),
            },
        );
        cache.observe_response(&agent, request);
        cache.update(
            &mut rows,
            &snapshot,
            &projection,
            EntryView {
                agent: &agent,
                view: &view,
                thinking: false,
                all_details: false,
            },
            &outputs,
            0,
        );
        let expected = entries(
            &snapshot,
            &projection,
            &agent,
            &view,
            &outputs,
            false,
            false,
        );
        assert!(rows == expected);
        assert!(
            rows.iter()
                .filter(|entry| entry.surface == Surface::Reasoning)
                .all(|entry| !entry.running)
        );
    }

    #[test]
    fn cached_reconnecting_indicator_tracks_activity_changes() {
        let root = AgentId::root(SessionId::from_bytes([1; 16]));
        let mut snapshot = ObservationSnapshot::default();
        let projection = Projection::default();
        let view = View::default();
        let outputs = HashMap::new();
        let mut cache = ContentCache::default();
        let mut rows = Vec::new();
        for activity in [
            AgentActivity::Working,
            AgentActivity::Reconnecting {
                attempt: 2,
                max_attempts: 3,
            },
            AgentActivity::Reconnecting {
                attempt: 3,
                max_attempts: 3,
            },
            AgentActivity::Interrupted,
        ] {
            update(
                &mut snapshot,
                RuntimeEvent::Activity {
                    agent: root.clone(),
                    activity,
                },
            );
            cache.update(
                &mut rows,
                &snapshot,
                &projection,
                EntryView {
                    agent: &root,
                    view: &view,
                    thinking: false,
                    all_details: false,
                },
                &outputs,
                0,
            );
            let expected = entries(&snapshot, &projection, &root, &view, &outputs, false, false);
            assert!(rows == expected);
        }
        assert!(rows.is_empty());
    }
}

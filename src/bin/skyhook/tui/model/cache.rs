use super::entries::{Dep, History, Inputs};
use super::live::{live_tail_response, live_tail_responses, response_entries, working_entry};
use super::requests::refresh_request_entry;
use super::{Entry, EntryView, Projection, Tab};
use crate::tui::app::OutputStore;
use skyhook::agent::ObservationSnapshot;
use skyhook::identity::{AgentId, JobId};
use skyhook::session::RequestSeq;
use std::collections::{HashMap, HashSet};

/// Retains rendered journal content across journal records and streaming events.
/// History folds new records and rebuilds only the entries whose sources changed;
/// the live tail and working indicator are rebuilt on each update. The caller
/// bumps `revision` for presentation changes, which rebuild everything, as do a
/// new agent, tab or detail setting.
///
/// Entries and all indices share one owner. Render/export borrow the entries;
/// historical strings and documents are never cloned at the update boundary.
#[derive(Default)]
pub struct ContentCache {
    entries: Vec<Entry>,
    overlay_len: usize,
    identity: Option<(AgentId, Tab, bool, u64)>,
    history: History,
    /// Requests whose responses form the live tail.
    live: Vec<RequestSeq>,
    dirty_responses: HashMap<AgentId, HashSet<RequestSeq>>,
    invalid_jobs: HashSet<JobId>,
}

impl ContentCache {
    /// Response events can replace content, reorder blocks, or end a block
    /// without changing its text. Never infer cache validity from string lengths.
    /// `request` is the runtime event's request sequence.
    pub fn observe_response(&mut self, agent: &AgentId, request: RequestSeq) {
        self.dirty_responses
            .entry(agent.clone())
            .or_default()
            .insert(request);
    }

    /// Refresh downloaded output for one tool without invalidating history.
    pub fn invalidate_job(&mut self, job: JobId) {
        self.invalid_jobs.insert(job);
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Returns the indices of entries that changed, including every entry past the
    /// previous length. Move-owned UI-only tail is never included in history/live/job
    /// indices.
    pub fn update(
        &mut self,
        snapshot: &ObservationSnapshot,
        projection: &mut Projection,
        presentation: EntryView<'_>,
        outputs: &OutputStore,
        revision: u64,
        overlay: Vec<Entry>,
    ) -> Vec<usize> {
        let mut entries = std::mem::take(&mut self.entries);
        let old_overlay_start = entries.len().saturating_sub(self.overlay_len);
        let old_overlay = entries.split_off(old_overlay_start);
        let mut dirty = self.update_retained(
            &mut entries,
            snapshot,
            projection,
            presentation,
            outputs,
            revision,
        );
        let start = entries.len();
        self.overlay_len = overlay.len();
        for (offset, entry) in overlay.into_iter().enumerate() {
            if start != old_overlay_start || old_overlay.get(offset) != Some(&entry) {
                dirty.push(start + offset);
            }
            entries.push(entry);
        }
        dirty.retain(|index| *index < entries.len());
        dirty.sort_unstable();
        dirty.dedup();
        self.entries = entries;
        dirty
    }

    fn update_retained(
        &mut self,
        entries: &mut Vec<Entry>,
        snapshot: &ObservationSnapshot,
        projection: &mut Projection,
        presentation: EntryView<'_>,
        outputs: &OutputStore,
        revision: u64,
    ) -> Vec<usize> {
        let mut changed = projection.take_changes();
        let projection = &*projection;
        let EntryView {
            agent, tab, view, ..
        } = presentation;
        let identity = (agent.clone(), tab, presentation.all_details, revision);
        let dirty_responses = self.dirty_responses.remove(agent).unwrap_or_default();
        changed.extend(dirty_responses.iter().copied().map(Dep::Request));
        changed.extend(self.invalid_jobs.drain().map(Dep::Job));
        let inputs = Inputs {
            snapshot,
            projection,
            presentation,
            outputs,
        };
        let rebuild = self.identity.as_ref() != Some(&identity);
        let old_len = self.history.len();
        let (old, mut dirty, shifted) = if rebuild {
            self.identity = Some(identity);
            let old = std::mem::take(entries);
            (self.history, *entries) = History::build(inputs);
            (old, Vec::new(), None)
        } else {
            let tail = entries.split_off(old_len);
            let update = self.history.update(inputs, entries, changed);
            (tail, update.dirty, update.shifted)
        };
        if tab == Tab::Requests {
            // Elapsed time changes without a new journal record.
            if let Some(request) = projection.ledger.open(agent)
                && let Some(record) = projection.ledger.get(request)
                && let Some(range) = self.history.record_entries(request.into())
                && let Some(entry) = entries[range.clone()].first_mut()
                && refresh_request_entry(entry, record)
            {
                dirty.push(range.start);
            }
        }
        if tab == Tab::Conversation {
            // Native events can replace equal-length text, reorder items, or end
            // blocks: the live suffix is rebuilt whole, never journal entries.
            let agent_name = projection.agent_name(agent);
            let requests: Vec<_> = if rebuild {
                let responses = live_tail_responses(snapshot, projection, agent);
                responses.into_iter().map(|(request, _)| request).collect()
            } else {
                let mut requests: Vec<_> =
                    self.live.iter().copied().chain(dirty_responses).collect();
                requests.sort_unstable();
                requests.dedup();
                requests
            };
            self.live.clear();
            for request in requests {
                if let Some(response) = live_tail_response(snapshot, projection, agent, request) {
                    self.live.push(request);
                    entries.extend(response_entries(request, response, view, agent_name));
                }
            }
            // The working indicator is a synthetic tail, never part of history.
            let running = entries.iter().any(|entry| entry.running);
            entries.extend(working_entry(snapshot, projection, agent, running));
        }
        if rebuild {
            // Unchanged entries keep their layout. An insertion or removal shifts
            // every later entry, so the changed suffix is relaid out.
            dirty.extend(changed_indices(&old, entries, 0));
        } else if let Some(from) = shifted {
            dirty.extend(from..entries.len());
        } else {
            // A reply committing to history takes the place of its live entries.
            dirty.extend(changed_indices(&old, &entries[old_len..], old_len));
        }
        dirty
    }
}

/// Indices (from `offset`) of entries which differ from, or extend past, `old`.
fn changed_indices(old: &[Entry], new: &[Entry], offset: usize) -> Vec<usize> {
    let changed = |index: &usize| old.get(*index) != new.get(*index);
    let indices = (0..new.len()).filter(changed);
    indices.map(|index| offset + index).collect()
}

#[cfg(test)]
mod tests {
    use super::super::entries::entries as history_entries;
    use super::super::tests::{Journal, job_info, replay, root, update};
    use super::super::{EntryKey, ResponseRef, Surface, Title, View};
    use super::*;
    use skyhook::agent::{AgentActivity, RuntimeEvent};
    use skyhook::job::{JobRole, JobState};
    use skyhook::provider::protocol::{
        AssistantItem, BlockId, BlockRef, Completion, ItemId, ResponseEvent,
    };
    use skyhook::session::SessionEvent;
    use skyhook::session::{Message, UserPart};

    fn show<'a>(agent: &'a AgentId, view: &'a View, all_details: bool) -> EntryView<'a> {
        EntryView {
            agent,
            tab: Tab::Conversation,
            view,
            all_details,
        }
    }

    /// Refresh the cache and check it matches a fresh uncached projection.
    fn refresh(
        cache: &mut ContentCache,
        (snapshot, projection): (&ObservationSnapshot, &mut Projection),
        presentation: EntryView,
        outputs: &OutputStore,
        revision: u64,
    ) -> Vec<usize> {
        let dirty = cache.update(
            snapshot,
            projection,
            presentation,
            outputs,
            revision,
            Vec::new(),
        );
        assert!(
            cache.entries() == history_entries(snapshot, projection, presentation, outputs, true),
            "the retained history differs from a fresh build"
        );
        dirty
    }

    #[tokio::test]
    async fn synchronous_failure_refreshes_the_existing_call_after_interleaved_user_input() {
        let mut journal = Journal::new().await;
        let agent = journal.agent();
        journal.call_record(&agent, "call").await;
        let mut projection = Projection::default();
        projection.rebuild(&journal.snapshot);
        let (mut cache, outputs, mut view) = Default::default();
        refresh(
            &mut cache,
            (&journal.snapshot, &mut projection),
            show(&agent, &view, false),
            &outputs,
            0,
        );
        let key = cache.entries()[0].key().clone();
        assert!(!cache.entries()[0].text().contains("Failed"));
        let text = "Continue after permission".into();
        let message = Message::User(vec![UserPart::Text { text }]);
        journal
            .record(&agent, SessionEvent::MessageCommitted { message })
            .await;
        journal.result_record(&agent, "call", true).await;
        let records_before = serde_json::to_value(&journal.snapshot.records).unwrap();
        projection.rebuild(&journal.snapshot);
        let dirty = refresh(
            &mut cache,
            (&journal.snapshot, &mut projection),
            show(&agent, &view, false),
            &outputs,
            0,
        );
        assert_eq!(dirty, [0, 1]);
        let entry = &cache.entries()[0];
        assert_eq!(cache.entries().len(), 2);
        assert_eq!(
            (entry.key(), entry.surface, entry.job_id()),
            (&key, Surface::Tool, None)
        );
        assert_eq!(entry.text(), "▸ × exec · Failed");
        view.set_expanded(key.clone(), true);
        refresh(
            &mut cache,
            (&journal.snapshot, &mut projection),
            show(&agent, &view, false),
            &outputs,
            1,
        );
        let text = cache.entries()[0].text();
        assert_eq!(cache.entries()[0].key(), &key);
        for part in [
            "Arguments",
            "Output\n  Permission was denied",
            "permission_denied",
        ] {
            assert!(text.contains(part), "{text}");
        }
        assert_eq!(text.matches("Permission was denied").count(), 1);
        assert_eq!(
            records_before,
            serde_json::to_value(&journal.snapshot.records).unwrap()
        );
    }

    #[test]
    fn content_cache_refreshes_only_invalidated_tool_output() {
        let agent = root(34);
        let snapshot = ObservationSnapshot::default();
        let mut projection = Projection::default();
        for id in 1..=2 {
            let job = job_info(&agent, id, JobRole::Tool, JobState::Completed);
            projection.jobs.insert(job.id, job);
        }
        let view = View::default();
        let presentation = EntryView {
            tab: Tab::Jobs,
            ..show(&agent, &view, true)
        };
        let (mut outputs, mut cache) = (OutputStore::default(), ContentCache::default());
        let state = (&snapshot, &mut projection);
        refresh(&mut cache, state, presentation, &outputs, 0);
        assert_eq!(cache.entries().len(), 2);
        let unchanged = cache.entries()[1].clone();
        let job = JobId::new(1).unwrap();
        let output = serde_json::json!({"stdout": "new output", "exit_code": 0});
        outputs.insert_product(job, crate::tui::tool_view::OutputView::historical(output));
        cache.invalidate_job(job);
        let state = (&snapshot, &mut projection);
        let dirty = refresh(&mut cache, state, presentation, &outputs, 0);
        assert_eq!(dirty, [0]);
        assert!(cache.entries()[0].text().contains("new output"));
        assert!(cache.entries().iter().all(|entry| entry.compact_after));
        assert!(cache.entries()[1] == unchanged);
    }

    #[tokio::test]
    async fn content_cache_reasoning_matches_uncached_across_shape_changes() {
        let mut journal = Journal::new().await;
        let agent = journal.agent();
        let request = journal.request(&agent, None).await.request;
        let mut projection = Projection::default();
        projection.rebuild(&journal.snapshot);
        let (view, outputs, mut cache) = Default::default();
        let sync = |cache: &mut ContentCache, snapshot: &_, projection: &mut _| {
            cache.observe_response(&agent, request);
            refresh(
                cache,
                (snapshot, projection),
                show(&agent, &view, false),
                &outputs,
                0,
            );
        };
        sync(&mut cache, &journal.snapshot, &mut projection);
        for text in ["\n", "first", "\n", "second", "\r\n", "third", "\n\n"] {
            journal.delta(&agent, request, "reasoning", text);
            sync(&mut cache, &journal.snapshot, &mut projection);
        }
        journal.delta(&agent, request, "text", "answer");
        sync(&mut cache, &journal.snapshot, &mut projection);
        // An equal-length authoritative replacement at the end must invalidate entries.
        let provisional = journal.snapshot.responses[&(agent.clone(), request)].blocks()[0]
            .text
            .clone();
        let replacement = provisional.replace("first", "FIRST");
        assert_eq!(replacement.len(), provisional.len());
        let (item, block) = (String::from("reasoning"), String::from("reasoning:0"));
        let ended = Completion::answer(vec![
            AssistantItem::reasoning("reasoning", 0, replacement.clone(), Some(replay())),
            AssistantItem::text("text", 1, "answer"),
        ])
        .unwrap();
        journal.response(&agent, request, ResponseEvent::End(ended));
        sync(&mut cache, &journal.snapshot, &mut projection);
        assert!(!cache.entries()[0].running);
        assert_eq!(
            cache.entries()[0].title(),
            Some(&Title::disclosed("Reasoning", false))
        );
        let block_ref = |item: &str, block: &str| BlockRef {
            item: ItemId::try_from(item.to_owned()).unwrap(),
            block: BlockId::try_from(block.to_owned()).unwrap(),
        };
        let response_ref = ResponseRef::Request(request);
        let keys = vec![
            EntryKey::Block {
                response: response_ref,
                block: block_ref(&item, &block),
            },
            EntryKey::Block {
                response: response_ref,
                block: block_ref("text", "text:0"),
            },
        ];
        // The working indicator trails the response until activity settles.
        let cached = |cache: &ContentCache| {
            let keys = cache.entries().iter().map(|entry| entry.key().clone());
            keys.filter(|key| !matches!(key, EntryKey::Working(_)))
                .collect::<Vec<_>>()
        };
        assert_eq!(cached(&cache), keys);
        let message = Message::Assistant(vec![
            AssistantItem::reasoning("reasoning", 0, replacement, Some(replay())),
            AssistantItem::text("text", 1, "answer"),
        ]);
        journal
            .record(&agent, SessionEvent::MessageCommitted { message })
            .await;
        projection.rebuild(&journal.snapshot);
        sync(&mut cache, &journal.snapshot, &mut projection);
        assert_eq!(cached(&cache), keys);
    }

    #[test]
    fn cached_reconnecting_indicator_tracks_activity_changes() {
        let agent = root(1);
        let mut snapshot = ObservationSnapshot::default();
        let (mut projection, view, outputs, mut cache) = Default::default();
        let reconnecting = |attempt| AgentActivity::Reconnecting { attempt };
        for activity in [
            AgentActivity::Working,
            reconnecting(2),
            reconnecting(3),
            AgentActivity::Stopped(skyhook::agent::TurnFailure::Interrupted),
        ] {
            let event = RuntimeEvent::Activity {
                agent: agent.clone(),
                activity,
            };
            update(&mut snapshot, event);
            refresh(
                &mut cache,
                (&snapshot, &mut projection),
                show(&agent, &view, false),
                &outputs,
                0,
            );
        }
        assert!(cache.entries().is_empty());
    }

    #[tokio::test]
    async fn retry_error_lifecycle_matches_fresh_rendering_and_replay() {
        let mut journal = Journal::new().await;
        let agent = journal.agent();
        let request = journal.request(&agent, None).await.request;
        let (view, outputs, mut cache) = Default::default();
        let mut projection = Projection::default();
        let presentation = show(&agent, &view, false);
        macro_rules! commit {
            ($event:expr) => {{
                journal.record(&agent, $event).await;
                projection.rebuild(&journal.snapshot);
                refresh(
                    &mut cache,
                    (&journal.snapshot, &mut projection),
                    presentation,
                    &outputs,
                    0,
                )
            }};
        }
        let replayed = |snapshot: &ObservationSnapshot| {
            let mut replay = ObservationSnapshot::default();
            for event in snapshot.records.values() {
                update(&mut replay, RuntimeEvent::Record(Box::new(event.clone())));
            }
            let mut projection = Projection::default();
            projection.rebuild(&replay);
            history_entries(&replay, &projection, presentation, &outputs, true)
        };
        for attempt in 1..=3 {
            // The request fixture already started attempt 1.
            if attempt == 1 {
                projection.rebuild(&journal.snapshot);
                refresh(
                    &mut cache,
                    (&journal.snapshot, &mut projection),
                    presentation,
                    &outputs,
                    0,
                );
            } else {
                let started = SessionEvent::ModelAttemptStarted(skyhook::session::AttemptRef {
                    request,
                    attempt,
                });
                commit!(started);
            }
            assert_eq!(cache.entries().len(), 1);
            assert_ne!(cache.entries()[0].key(), &EntryKey::Retry(request));
            let text = cache.entries()[0].text();
            assert!(
                ["attempt", "partial", "HTTP"]
                    .iter()
                    .all(|part| !text.contains(part))
            );
            for (index, suffix) in [format!("partial {attempt}"), " updated".into()]
                .into_iter()
                .enumerate()
            {
                journal.delta(&agent, request, "text", &suffix);
                cache.observe_response(&agent, request);
                let dirty = refresh(
                    &mut cache,
                    (&journal.snapshot, &mut projection),
                    presentation,
                    &outputs,
                    0,
                );
                // The first delta pushes the working indicator below the reply.
                let expected = if index == 0 { vec![0, 1] } else { vec![0] };
                assert_eq!(dirty, expected, "delta {index}");
                assert!(
                    cache
                        .entries()
                        .iter()
                        .all(|entry| !entry.text().contains("attempt"))
                );
            }
            if attempt == 3 {
                break;
            }
            let error = String::from("HTTP 503 [code=overloaded]");
            let failed = SessionEvent::ModelFailed {
                attempt: skyhook::session::AttemptRef { request, attempt },
                error: error.clone(),
                kind: skyhook::session::ModelFailureKind::Error,
            };
            assert_eq!(commit!(failed), [0]);
            assert_eq!(cache.entries().len(), 1);
            assert_eq!(cache.entries()[0].key(), &EntryKey::Retry(request));
            let failure = *journal.snapshot.records.keys().next_back().unwrap();
            let scheduled = SessionEvent::ModelRecoveryScheduled {
                failure,
                delay_millis: 1000,
            };
            assert_eq!(commit!(scheduled), [0]);
            assert_eq!(cache.entries().len(), 1);
            let text = cache.entries()[0].text();
            let retrying = format!("Retrying · attempt {}", attempt + 1);
            for part in [retrying.as_str(), "HTTP 503", "retry delay 1000 ms"] {
                assert!(text.contains(part), "{text}");
            }
            assert!(text.ends_with(&format!("partial {attempt} updated")));
        }
        let message = Message::Assistant(vec![AssistantItem::text("answer", 0, "final answer")]);
        let committed = SessionEvent::MessageCommitted { message };
        commit!(committed);
        assert_eq!(cache.entries().len(), 1);
        let committed = cache.entries()[0].clone();
        let text = committed.text();
        assert!(
            text.ends_with("final answer")
                && !text.contains("attempt")
                && !text.contains("partial")
        );
        assert!(committed.footer.is_some());
        assert!(cache.entries() == replayed(&journal.snapshot));

        // A provider abort commits its visible text before publishing ModelFailed:
        // the message stays intact beside the diagnostics, including on replay.
        let error = "provider aborted response".into();
        let failed = SessionEvent::ModelFailed {
            attempt: skyhook::session::AttemptRef {
                request,
                attempt: 3,
            },
            error,
            kind: skyhook::session::ModelFailureKind::Error,
        };
        commit!(failed);
        assert_eq!(cache.entries().len(), 2);
        assert!(cache.entries().contains(&committed));
        let failures: Vec<_> = cache
            .entries()
            .iter()
            .filter(|card| card.key() == &EntryKey::Retry(request))
            .map(Entry::text)
            .collect();
        assert_eq!(
            failures,
            ["Request failed · attempt 3\nprovider aborted response"]
        );
        assert!(cache.entries() == replayed(&journal.snapshot));
    }

    /// Every tab's retained history matches a fresh build after each record of a
    /// mixed journal, and conversation updates never relay out its first entry.
    #[tokio::test]
    async fn incremental_history_matches_a_fresh_build_record_by_record() {
        use skyhook::job::{AgentMessage, JobEnd, JobTransition};
        use skyhook::provider::protocol::{ToolCall, ToolResult};
        use skyhook::session::{
            AttemptRef, CompletedOutcome, JobEvent, ModelCallOrigin, ModelFailureKind,
        };
        let mut journal = Journal::new().await;
        let agent = journal.agent();
        let (view, outputs) = (View::default(), OutputStore::default());
        let mut tabs = [Tab::Conversation, Tab::Requests, Tab::Jobs]
            .map(|tab| (tab, Projection::default(), ContentCache::default()));
        // `streamed` requests had response events since the previous step.
        let mut step = |journal: &Journal, label: &str, streamed: &[RequestSeq]| {
            for (tab, projection, cache) in &mut tabs {
                for request in streamed {
                    cache.observe_response(&agent, *request);
                }
                projection.rebuild(&journal.snapshot);
                let presentation = EntryView {
                    tab: *tab,
                    ..show(&agent, &view, false)
                };
                let first = cache.entries().first().cloned();
                let state = (&journal.snapshot, &mut *projection);
                let dirty = refresh(cache, state, presentation, &outputs, 0);
                if *tab == Tab::Conversation && first.is_some() {
                    assert!(!dirty.contains(&0), "{label}: {dirty:?}");
                }
            }
        };
        let committed = |message| SessionEvent::MessageCommitted { message };
        let user = |text: &str| Message::User(vec![UserPart::Text { text: text.into() }]);
        journal.record(&agent, committed(user("question"))).await;
        step(&journal, "user message", &[]);
        let working = RuntimeEvent::Activity {
            agent: agent.clone(),
            activity: AgentActivity::Working,
        };
        update(&mut journal.snapshot, working);
        step(&journal, "working", &[]);
        let request = journal.request(&agent, None).await.request;
        step(&journal, "requested", &[]);
        journal.delta(&agent, request, "text", "partial");
        step(&journal, "delta", &[request]);
        let attempt = |attempt| AttemptRef { request, attempt };
        let error = "HTTP 503".into();
        let failure = SessionEvent::ModelFailed {
            attempt: attempt(1),
            error,
            kind: ModelFailureKind::Error,
        };
        let failure = journal.record(&agent, failure).await;
        step(&journal, "failed", &[]);
        let recovery = SessionEvent::ModelRecoveryScheduled {
            failure,
            delay_millis: 10,
        };
        journal.record(&agent, recovery).await;
        step(&journal, "retrying", &[]);
        journal
            .record(&agent, SessionEvent::ModelAttemptStarted(attempt(2)))
            .await;
        step(&journal, "second attempt", &[]);
        let call = |id, tool, position: u32| {
            let call = ToolCall::new(id, tool, serde_json::json!({"command": ["true"]})).unwrap();
            AssistantItem::tool_call(id, position, call)
        };
        let reply = Message::Assistant(vec![
            AssistantItem::text("text", 0, "working on it"),
            call("admitted", "exec", 1),
            call("script", "script", 2),
            call("parallel", "script", 3),
            call("plain", "exec", 4),
        ]);
        let message = journal.record(&agent, committed(reply)).await.message();
        step(&journal, "calls committed", &[]);
        let completed = SessionEvent::ResponseCompleted {
            attempt: attempt(2),
            message,
            outcome: CompletedOutcome::Answer,
        };
        journal.record(&agent, completed).await;
        step(&journal, "response completed", &[]);
        let job = |id: u64, tool: &str, role, parent: Option<u64>, origin: Option<&str>| {
            let origin = origin.map(|call| ModelCallOrigin {
                message,
                call_id: call.into(),
            });
            SessionEvent::JobCreated {
                job: JobId::new(id).unwrap(),
                parent: parent.map(|parent| JobId::new(parent).unwrap()),
                origin,
                tool: tool.into(),
                role,
                name: None,
                arguments: serde_json::json!({"command": ["true"]}),
                output_schema: None,
                accepts_input: false,
                background: false,
                location: skyhook::execution::ExecutionLocation::root("/workspace".into()),
            }
        };
        // Parallel scripts' children interleave in the journal.
        let created = [
            job(1, "exec", JobRole::Tool, None, Some("admitted")),
            job(2, "script", JobRole::Script, None, Some("script")),
            job(4, "script", JobRole::Script, None, Some("parallel")),
            job(3, "exec", JobRole::Tool, Some(2), None),
            job(5, "exec", JobRole::Tool, Some(4), None),
            job(6, "exec", JobRole::Tool, Some(2), None),
        ];
        for (index, created) in created.into_iter().enumerate() {
            journal.record(&agent, created).await;
            step(&journal, &format!("job {index} created"), &[]);
        }
        let running = SessionEvent::JobStateChanged {
            job: JobId::new(1).unwrap(),
            state: JobTransition::Running,
        };
        journal.record(&agent, running).await;
        step(&journal, "job running", &[]);
        for id in [3, 5, 6, 1, 2, 4] {
            let finished = SessionEvent::JobFinished {
                job: JobId::new(id).unwrap(),
                state: JobEnd::Completed,
                diagnostic: None,
                output_diagnostic: None,
                images: vec![],
            };
            journal.record(&agent, finished).await;
            step(&journal, &format!("job {id} finished"), &[]);
        }
        let result = |id: &str, name: &str| ToolResult {
            call_id: id.into(),
            name: name.into(),
            result: serde_json::json!({"stdout": "done"}),
            images: vec![],
            is_error: false,
        };
        for (id, name) in [
            ("admitted", "exec"),
            ("script", "script"),
            ("parallel", "script"),
            ("plain", "exec"),
        ] {
            let results = Message::Tool(vec![result(id, name)]);
            journal.record(&agent, committed(results)).await;
            step(&journal, id, &[]);
        }
        let notice = JobEvent::Message(AgentMessage {
            id: JobId::new(2).unwrap(),
            name: None,
            message,
            text: "script says hi".into(),
        });
        let events = Message::User(vec![UserPart::JobEvents {
            events: vec![notice],
        }]);
        journal.record(&agent, committed(events)).await;
        step(&journal, "job events", &[]);
        let next = journal.request(&agent, None).await.request;
        journal.delta(&agent, next, "text", "cut short");
        step(&journal, "second request streaming", &[next]);
        let interrupted = SessionEvent::ModelAttemptInterrupted(AttemptRef {
            request: next,
            attempt: 1,
        });
        journal.record(&agent, interrupted).await;
        step(&journal, "interrupted", &[]);
        let stopped = RuntimeEvent::Activity {
            agent: agent.clone(),
            activity: AgentActivity::Stopped(skyhook::agent::TurnFailure::Interrupted),
        };
        update(&mut journal.snapshot, stopped);
        // Stopping settles the interrupted response, moving it to its journal position.
        step(&journal, "stopped", &[next]);
        let status = SessionEvent::Status {
            message: "done".into(),
        };
        journal.record(&agent, status).await;
        step(&journal, "status", &[]);

        // Each script's children sit directly under it, indented, on both tabs,
        // and the response's tool cards stay one tight block.
        let tree = [(1, 0), (2, 0), (3, 2), (6, 2), (4, 0), (5, 2)]
            .map(|(id, indent)| (JobId::new(id).unwrap(), indent));
        let cards = |entries: &[Entry]| -> Vec<_> {
            let cards = entries.iter().filter_map(|e| Some((e.job_id()?, e.indent)));
            cards.collect()
        };
        let entries = tabs[0].2.entries();
        let first = entries.iter().position(|entry| entry.job_id().is_some());
        let block = &entries[first.unwrap()..][..6];
        assert_eq!(cards(block), tree);
        assert!(block[..5].iter().all(|entry| entry.compact_after));
        assert_eq!(cards(tabs[2].2.entries()), tree);
    }

    #[tokio::test]
    async fn retained_owner_separates_overlay_and_keeps_replacement_indices_valid() {
        let mut journal = Journal::new().await;
        let agent = journal.agent();
        let (mut projection, view, outputs, mut cache) = Default::default();
        let presentation = show(&agent, &view, false);
        let sync =
            |cache: &mut ContentCache, snapshot: &_, projection: &mut _, text: Option<&str>| {
                let overlay = text.map(|text| {
                    Entry::new(EntryKey::UnsavedStatus(0), text.into(), Surface::Status)
                });
                let overlay = overlay.into_iter().collect();
                cache.update(snapshot, projection, presentation, &outputs, 0, overlay)
            };
        assert_eq!(
            sync(
                &mut cache,
                &journal.snapshot,
                &mut projection,
                Some("first")
            ),
            [0]
        );
        assert_eq!(cache.entries().len(), 1);
        let dirty = sync(
            &mut cache,
            &journal.snapshot,
            &mut projection,
            Some("other"),
        );
        assert_eq!(dirty, [0]);
        assert_eq!(cache.entries()[0].text(), "other");
        journal.call_record(&agent, "call").await;
        projection.rebuild(&journal.snapshot);
        sync(
            &mut cache,
            &journal.snapshot,
            &mut projection,
            Some("other"),
        );
        assert_eq!(cache.entries().len(), 2);
        assert_eq!(cache.entries()[1].key(), &EntryKey::UnsavedStatus(0));
        sync(&mut cache, &journal.snapshot, &mut projection, None);
        assert_eq!(cache.entries().len(), 1);
        // Switching to an empty tab invalidates retained history and old overlay.
        let presentation = EntryView {
            tab: Tab::Requests,
            ..show(&agent, &view, false)
        };
        cache.update(
            &journal.snapshot,
            &mut projection,
            presentation,
            &outputs,
            1,
            Vec::new(),
        );
        assert!(cache.entries().is_empty());
    }
}

use super::entries::entries as history_entries;
use super::jobs::job_entry;
use super::live::{response_entries, working_entry};
use super::requests::{refresh_request_entry, request_running};
use super::{Entry, EntryKey, EntryView, Projection, Tab};
use crate::tui::app::OutputStore;
use skyhook::agent::ObservationSnapshot;
use skyhook::identity::{AgentId, JobId};
use std::collections::{HashMap, HashSet};

/// Dirty replacement indices refer to the retained owner's entries.
/// No append witness is inferred from lengths or invalidation revisions.
#[derive(Default, Debug)]
pub struct ContentChanges {
    pub dirty: Vec<usize>,
    pub reset: bool,
}

/// Retains rendered journal content across streaming events. The caller bumps
/// `revision` for history and presentation changes. ResponseEvent mutations use
/// `observe_response` so authoritative replacements rebuild only the live tail.
/// Journal progress, selected agent, tab and defaults are also checked here.
///
/// Entries and all indices share one owner. Render/export borrow the entries;
/// historical strings and documents are never cloned at the update boundary.
#[derive(Default)]
pub struct ContentCache {
    entries: Vec<Entry>,
    overlay_len: usize,
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
            if let Some(job) = entry.job_id() {
                self.job_indices.insert(job, index);
            }
        }
        self.invalid_jobs.clear();
        // Preserve warm layout for journal/output changes that leave ordering
        // intact, including response snapshot replacements.
        changes.reset =
            old.is_empty() || old.iter().zip(entries.iter()).any(|(a, b)| a.key != b.key);
        changes.dirty.clear();
        if !changes.reset {
            changes.dirty = changed_indices(&old, entries, 0);
        }
    }

    fn push_live(
        &mut self,
        entries: &mut Vec<Entry>,
        request: u64,
        content: impl IntoIterator<Item = Entry>,
    ) {
        let start = entries.len();
        entries.extend(content);
        let count = entries.len() - start;
        self.live.push(LiveContent {
            request,
            start,
            count,
        });
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Move-owned UI-only tail is never included in history/live/job indices.
    pub fn update(
        &mut self,
        snapshot: &ObservationSnapshot,
        projection: &Projection,
        presentation: EntryView<'_>,
        outputs: &OutputStore,
        revision: u64,
        overlay: Vec<Entry>,
    ) -> ContentChanges {
        let mut entries = std::mem::take(&mut self.entries);
        let old_overlay_start = entries.len().saturating_sub(self.overlay_len);
        let old_overlay = entries.split_off(old_overlay_start);
        let mut changes = self.update_retained(
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
                changes.dirty.push(start + offset);
            }
            entries.push(entry);
        }
        changes.dirty.retain(|index| *index < entries.len());
        changes.dirty.sort_unstable();
        changes.dirty.dedup();
        self.entries = entries;
        changes
    }

    fn update_retained(
        &mut self,
        entries: &mut Vec<Entry>,
        snapshot: &ObservationSnapshot,
        projection: &Projection,
        presentation: EntryView<'_>,
        outputs: &OutputStore,
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
        let agent_name = projection.agent_name(agent);
        if reset {
            self.identity = Some(identity);
            *entries = history_entries(snapshot, projection, presentation, outputs, false);
            self.history_len = entries.len();
            self.history_running = entries.iter().any(|entry| entry.running);
            self.live.clear();
        }
        if !reset && view.tab == Tab::Conversation {
            // Retry cards live at their original journal position. Replace only
            // the affected card, including authoritative equal-length updates.
            for request in &dirty_responses {
                if let Some(entry) =
                    super::retry::retry_entry(snapshot, projection, agent, *request, thinking)
                    && let Some(index) = entries[..self.history_len]
                        .iter()
                        .position(|old| old.key() == entry.key())
                    && entries[index] != entry
                {
                    entries[index] = entry;
                    changes.dirty.push(index);
                }
            }
            self.history_running = entries[..self.history_len]
                .iter()
                .any(|entry| entry.running);
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
                    .position(|entry| entry.key() == &EntryKey::Request(request))
            {
                let running = request_running(info, snapshot, projection, agent, request);
                if refresh_request_entry(&mut entries[index], info, running) {
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
            let responses = super::live::live_tail_responses(snapshot, projection, agent);
            for (request, response) in responses {
                let content = response_entries(request, response, view, thinking, agent_name);
                self.push_live(entries, request, content);
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
                if let Some(response) =
                    super::live::live_tail_response(snapshot, projection, agent, request)
                {
                    let content = response_entries(request, response, view, thinking, agent_name);
                    self.push_live(entries, request, content);
                }
            }
            // The old working indicator is regenerated below; entry removal is
            // conveyed by vector length.
            let live = &entries[self.history_len..];
            let mut tail = previous.iter().zip(live);
            changes.reset |= tail.any(|(old, new)| old.key() != new.key());
            let dirty = changed_indices(&previous, live, self.history_len);
            changes.dirty.extend(dirty);
        }
        // The working indicator is a synthetic tail, never part of history.
        let end = self
            .live
            .last()
            .map_or(self.history_len, |l| l.start + l.count);
        let working_key = EntryKey::Working(agent.clone());
        let running = self.history_running
            || entries[self.history_len..end]
                .iter()
                .any(|entry| entry.running);
        let has_working = entries.get(end).is_some_and(|e| e.key() == &working_key);
        if let Some(entry) = working_entry(snapshot, projection, agent, running) {
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
        changes
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
    use super::super::tests::{
        call_record, delta, job_info, record, replay, request, response, result_record, root,
        update,
    };
    use super::super::{Surface, View};
    use super::*;
    use skyhook::agent::{AgentActivity, RuntimeEvent};
    use skyhook::job::{JobRole, JobState};
    use skyhook::provider::protocol::{
        AssistantItem, BlockContent, Message, ResponseEvent, UserContent,
    };
    use skyhook::session::SessionEvent;

    fn show<'a>(agent: &'a AgentId, view: &'a View, all_details: bool) -> EntryView<'a> {
        EntryView {
            agent,
            view,
            thinking: false,
            all_details,
        }
    }

    /// Refresh the cache and check it matches a fresh uncached projection.
    fn refresh(
        cache: &mut ContentCache,
        (snapshot, projection): (&ObservationSnapshot, &Projection),
        presentation: EntryView,
        outputs: &OutputStore,
        revision: u64,
    ) -> ContentChanges {
        let changes = cache.update(
            snapshot,
            projection,
            presentation,
            outputs,
            revision,
            Vec::new(),
        );
        assert!(
            cache.entries() == history_entries(snapshot, projection, presentation, outputs, true)
        );
        changes
    }

    #[test]
    fn synchronous_failure_refreshes_the_existing_call_after_interleaved_user_input() {
        let agent = root(71);
        let mut snapshot = ObservationSnapshot::default();
        call_record(&mut snapshot, &agent, "call");
        let mut projection = Projection::default();
        projection.rebuild(&snapshot);
        let (mut cache, outputs, mut view) = Default::default();
        refresh(
            &mut cache,
            (&snapshot, &projection),
            show(&agent, &view, false),
            &outputs,
            0,
        );
        let key = cache.entries()[0].key().clone();
        assert!(!cache.entries()[0].text().contains("Failed"));
        let text = "Continue after permission".into();
        let message = Message::User(vec![UserContent::Text { text }]);
        record(
            &mut snapshot,
            &agent,
            SessionEvent::MessageCommitted { message },
        );
        result_record(&mut snapshot, &agent, "call", true);
        let records_before = serde_json::to_value(&snapshot.records).unwrap();
        projection.rebuild(&snapshot);
        let changes = refresh(
            &mut cache,
            (&snapshot, &projection),
            show(&agent, &view, false),
            &outputs,
            0,
        );
        assert!(!changes.reset && changes.dirty.contains(&0));
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
            (&snapshot, &projection),
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
            serde_json::to_value(&snapshot.records).unwrap()
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
        let view = View {
            tab: Tab::Jobs,
            ..View::default()
        };
        let (mut outputs, mut cache) = (OutputStore::default(), ContentCache::default());
        let state = (&snapshot, &projection);
        refresh(&mut cache, state, show(&agent, &view, true), &outputs, 0);
        assert_eq!(cache.entries().len(), 2);
        let unchanged = cache.entries()[1].clone();
        let job = JobId::new(1).unwrap();
        let output = serde_json::json!({"stdout": "new output", "exit_code": 0});
        outputs.insert_product(job, crate::tui::tool_view::OutputView::historical(output));
        cache.invalidate_job(job);
        let changes = refresh(&mut cache, state, show(&agent, &view, true), &outputs, 0);
        assert!(!changes.reset);
        assert_eq!(changes.dirty, vec![0]);
        assert!(cache.entries()[0].text().contains("new output"));
        assert!(cache.entries().iter().all(|entry| entry.compact_after));
        assert!(cache.entries()[1] == unchanged);
    }

    #[test]
    fn content_cache_reasoning_matches_uncached_across_shape_changes() {
        let agent = root(32);
        let mut snapshot = ObservationSnapshot::default();
        let request = request(&mut snapshot, &agent, None);
        let mut projection = Projection::default();
        projection.rebuild(&snapshot);
        let (view, outputs, mut cache) = Default::default();
        let sync = |cache: &mut ContentCache, snapshot: &_, projection: &_| {
            cache.observe_response(&agent, request);
            refresh(
                cache,
                (snapshot, projection),
                show(&agent, &view, false),
                &outputs,
                0,
            );
        };
        sync(&mut cache, &snapshot, &projection);
        for text in ["\n", "first", "\n", "second", "\r\n", "third", "\n\n"] {
            delta(&mut snapshot, &agent, request, "reasoning", text);
            sync(&mut cache, &snapshot, &projection);
        }
        delta(&mut snapshot, &agent, request, "text", "answer");
        sync(&mut cache, &snapshot, &projection);
        // Equal-length authoritative replacement and block-only closure must invalidate entries.
        let provisional = snapshot.responses[&(agent.clone(), request)]
            .snapshot()
            .items[0]
            .blocks[0]
            .text
            .clone();
        let replacement = provisional.replace("first", "FIRST");
        assert_eq!(replacement.len(), provisional.len());
        let (item, block) = (String::from("reasoning"), String::from("reasoning:0"));
        let text = replacement.clone();
        for event in [
            ResponseEvent::BlockEnded {
                item: item.clone(),
                block: block.clone(),
                content: BlockContent::Reasoning { text },
            },
            ResponseEvent::ItemEnded {
                id: item.clone(),
                replay: Some(replay()),
            },
        ] {
            response(&mut snapshot, &agent, request, event);
        }
        sync(&mut cache, &snapshot, &projection);
        assert!(!cache.entries()[0].running);
        assert!(cache.entries()[0].text().starts_with("▸ Reasoning"));
        let keys = vec![
            EntryKey::ReasoningBlock {
                request,
                item,
                block,
            },
            EntryKey::ResponseBlock {
                request,
                item: "text".into(),
                block: "text:0".into(),
            },
        ];
        let cached = |cache: &ContentCache| {
            let keys = cache.entries().iter().map(|entry| entry.key().clone());
            keys.collect::<Vec<_>>()
        };
        assert_eq!(cached(&cache), keys);
        let message = Message::Assistant(vec![
            AssistantItem::reasoning("reasoning", 0, replacement, Some(replay())),
            AssistantItem::text("text", 1, "answer"),
        ]);
        record(
            &mut snapshot,
            &agent,
            SessionEvent::MessageCommitted { message },
        );
        projection.rebuild(&snapshot);
        sync(&mut cache, &snapshot, &projection);
        assert_eq!(cached(&cache), keys);
    }

    #[test]
    fn cached_reconnecting_indicator_tracks_activity_changes() {
        let agent = root(1);
        let mut snapshot = ObservationSnapshot::default();
        let (projection, view, outputs, mut cache) = Default::default();
        let reconnecting = |attempt| AgentActivity::Reconnecting { attempt };
        for activity in [
            AgentActivity::Working,
            reconnecting(2),
            reconnecting(3),
            AgentActivity::Interrupted,
        ] {
            let event = RuntimeEvent::Activity {
                agent: agent.clone(),
                activity,
            };
            update(&mut snapshot, event);
            refresh(
                &mut cache,
                (&snapshot, &projection),
                show(&agent, &view, false),
                &outputs,
                0,
            );
        }
        assert!(cache.entries().is_empty());
    }

    #[test]
    fn retry_error_lifecycle_matches_fresh_rendering_and_replay() {
        let agent = root(41);
        let mut snapshot = ObservationSnapshot::default();
        let request = request(&mut snapshot, &agent, None);
        let (view, outputs, mut cache, mut projection) = Default::default();
        let presentation = show(&agent, &view, false);
        let commit = |state: (&mut ObservationSnapshot, &mut Projection),
                      cache: &mut ContentCache,
                      event| {
            record(state.0, &agent, event);
            state.1.rebuild(state.0);
            refresh(cache, (state.0, state.1), presentation, &outputs, 0)
        };
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
            let started = SessionEvent::ModelAttemptStarted { request, attempt };
            commit((&mut snapshot, &mut projection), &mut cache, started);
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
                delta(&mut snapshot, &agent, request, "text", &suffix);
                cache.observe_response(&agent, request);
                let changes = refresh(
                    &mut cache,
                    (&snapshot, &projection),
                    presentation,
                    &outputs,
                    0,
                );
                assert!(index == 0 || !changes.reset);
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
                request,
                attempt,
                error: error.clone(),
                kind: skyhook::session::ModelFailureKind::Error,
            };
            assert!(commit((&mut snapshot, &mut projection), &mut cache, failed).reset);
            assert_eq!(cache.entries().len(), 1);
            assert_eq!(cache.entries()[0].key(), &EntryKey::Retry(request));
            let scheduled = SessionEvent::ModelRecoveryScheduled {
                request,
                attempt: attempt + 1,
                delay_millis: 1000,
                error,
            };
            assert!(!commit((&mut snapshot, &mut projection), &mut cache, scheduled).reset);
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
        commit((&mut snapshot, &mut projection), &mut cache, committed);
        assert_eq!(cache.entries().len(), 1);
        let committed = cache.entries()[0].clone();
        let text = committed.text();
        assert!(
            text.ends_with("final answer")
                && !text.contains("attempt")
                && !text.contains("partial")
        );
        assert!(committed.footer.is_some());
        assert!(cache.entries() == replayed(&snapshot));

        // A provider abort commits its visible text before publishing ModelFailed:
        // the message stays intact beside the diagnostics, including on replay.
        let error = "provider aborted response".into();
        let failed = SessionEvent::ModelFailed {
            request,
            attempt: 3,
            error,
            kind: skyhook::session::ModelFailureKind::Error,
        };
        commit((&mut snapshot, &mut projection), &mut cache, failed);
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
        assert!(cache.entries() == replayed(&snapshot));
    }

    #[test]
    fn retained_owner_separates_overlay_and_keeps_replacement_indices_valid() {
        let agent = root(99);
        let mut snapshot = ObservationSnapshot::default();
        let (mut projection, view, outputs, mut cache) = Default::default();
        let presentation = show(&agent, &view, false);
        let sync = |cache: &mut ContentCache, snapshot: &_, projection: &_, text: Option<&str>| {
            let overlay = text
                .map(|text| Entry::new(EntryKey::UnsavedStatus(0), text.into(), Surface::Status));
            let overlay = overlay.into_iter().collect();
            cache.update(snapshot, projection, presentation, &outputs, 0, overlay)
        };
        assert!(sync(&mut cache, &snapshot, &projection, Some("first")).reset);
        assert_eq!(cache.entries().len(), 1);
        let changes = sync(&mut cache, &snapshot, &projection, Some("other"));
        assert!(!changes.reset);
        assert_eq!(changes.dirty, vec![0]);
        assert_eq!(cache.entries()[0].text(), "other");
        call_record(&mut snapshot, &agent, "call");
        projection.rebuild(&snapshot);
        sync(&mut cache, &snapshot, &projection, Some("other"));
        assert_eq!(cache.entries().len(), 2);
        assert_eq!(cache.entries()[1].key(), &EntryKey::UnsavedStatus(0));
        sync(&mut cache, &snapshot, &projection, None);
        assert_eq!(cache.entries().len(), 1);
        // Switching to an empty tab invalidates retained history and old overlay.
        let view = View {
            tab: Tab::Requests,
            ..View::default()
        };
        let presentation = show(&agent, &view, false);
        cache.update(
            &snapshot,
            &projection,
            presentation,
            &outputs,
            1,
            Vec::new(),
        );
        assert!(cache.entries().is_empty());
    }
}

use super::entries::{Dep, History, Inputs};
use super::live::{live_tail, live_tail_responses};
use super::{Entry, EntryKey, EntryView, Projection, Tab};
use crate::tui::app::OutputStore;
use skyhook::agent::{ObservationSnapshot, RuntimeEvent};
use skyhook::identity::{AgentId, JobId};
use skyhook::session::RequestSeq;
use std::collections::{HashMap, HashSet};

/// Retains rendered journal content across journal records and streaming events.
/// History folds new records and rebuilds only the entries whose sources changed;
/// the live tail and working indicator are rebuilt on each update. The caller
/// bumps `revision` for presentation changes, which rebuild everything, as do a
/// new agent, tab or detail setting, and a new day, since earlier times then show
/// their date; a toggled entry rebuilds only its source.
///
/// Entries and all indices share one owner. Render/export borrow the entries;
/// historical strings and documents are never cloned at the update boundary.
#[derive(Default)]
pub struct ContentCache {
    entries: Vec<Entry>,
    overlay_len: usize,
    identity: Option<Identity>,
    history: History,
    /// Requests whose responses form the live tail.
    live: Vec<RequestSeq>,
    dirty_responses: HashMap<AgentId, HashSet<RequestSeq>>,
    invalid: HashSet<Dep>,
}

/// What every entry was built for; any change rebuilds them all.
#[derive(PartialEq)]
struct Identity {
    agent: AgentId,
    tab: Tab,
    all_details: bool,
    revision: u64,
    today: chrono::NaiveDate,
}

impl ContentCache {
    /// Note the responses `event` changes: response events can replace content,
    /// reorder blocks, or end a block without changing its text. Never infer
    /// cache validity from string lengths.
    pub fn observe(&mut self, event: &RuntimeEvent) {
        if let RuntimeEvent::ResponseEvent { agent, request, .. }
        | RuntimeEvent::ResponseSettled { agent, request, .. } = event
        {
            self.observe_response(agent, *request);
        }
    }

    fn observe_response(&mut self, agent: &AgentId, request: RequestSeq) {
        self.dirty_responses
            .entry(agent.clone())
            .or_default()
            .insert(request);
    }

    /// Refresh downloaded output for one tool without invalidating history.
    pub fn invalidate_job(&mut self, job: JobId) {
        self.invalid.insert(Dep::Job(job));
    }

    /// Rebuild the entry after its expansion changed.
    pub fn invalidate_entry(&mut self, key: EntryKey) {
        self.invalid.insert(Dep::Entry(key));
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
            agent,
            tab,
            all_details,
            today,
            ..
        } = presentation;
        let identity = Identity {
            agent: agent.clone(),
            tab,
            all_details,
            revision,
            today,
        };
        let dirty_responses = self.dirty_responses.remove(agent).unwrap_or_default();
        changed.extend(dirty_responses.iter().copied().map(Dep::Request));
        changed.extend(self.invalid.drain());
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
        if tab == Tab::Conversation {
            // Native events can replace equal-length text, reorder items, or end
            // blocks: the live suffix is rebuilt whole, never journal entries.
            let requests: Vec<_> = if rebuild {
                live_tail_responses(snapshot, agent)
            } else {
                let mut requests: Vec<_> =
                    self.live.iter().copied().chain(dirty_responses).collect();
                requests.sort_unstable();
                requests.dedup();
                requests
            };
            self.live = live_tail(snapshot, projection, presentation, requests, entries);
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
    use super::super::tests::{loaded, replay, root, update};
    use super::super::{Clock, EntryKey, ResponseRef, Surface, Timing, Title, View};
    use super::*;
    use crate::tests::bounded;
    use crate::tui::app::{
        App,
        played::{
            activity, ask, assistant, at, call, committed, completed, count, failing_call,
            job_ended, job_events, job_of_call, latest_request, model_failed, observe_find,
            observe_next, observe_record, observe_until, played_fixture, reached, recorded,
            results, sleeping_script, status, stopped, streamed, tool_error,
        },
        tests::fixture,
    };
    use serde_json::json;
    use skyhook::agent::{AgentActivity, RuntimeEvent};
    use skyhook::job::JobTransition;
    use skyhook::provider::ProviderErrorKind;
    use skyhook::provider::protocol::{
        AssistantItem, BlockId, BlockRef, Completion, ItemId, ItemKind,
    };
    use skyhook::session::AttemptFailure;
    use skyhook::session::SessionEvent;

    /// One agent's tab folded into a cache from successive snapshots.
    struct Fold {
        agent: AgentId,
        tab: Tab,
        today: chrono::NaiveDate,
        all_details: bool,
        view: View,
        projection: Projection,
        cache: ContentCache,
        outputs: OutputStore,
        /// UI-only entries shown after the history.
        overlay: Vec<Entry>,
        /// Job views as the fold last saw them.
        jobs: std::collections::BTreeMap<JobId, skyhook::job::JobEnvelope>,
    }

    impl Fold {
        fn new(agent: &AgentId, tab: Tab) -> Self {
            Self {
                agent: agent.clone(),
                tab,
                today: Default::default(),
                all_details: false,
                view: View::default(),
                projection: Projection::default(),
                cache: ContentCache::default(),
                outputs: OutputStore::default(),
                overlay: Vec::new(),
                jobs: Default::default(),
            }
        }

        /// Fold `snapshot` in and check the cache matches a fresh uncached build.
        fn refresh(&mut self, snapshot: &ObservationSnapshot) -> Vec<usize> {
            // A changed job view arrives as its own update, before its record.
            for (id, job) in &snapshot.jobs {
                if self.jobs.get(id) != Some(job) {
                    self.projection.observe_job(job);
                }
            }
            self.jobs.clone_from(&snapshot.jobs);
            self.projection.rebuild(snapshot);
            let presentation = EntryView {
                agent: &self.agent,
                tab: self.tab,
                view: &self.view,
                all_details: self.all_details,
                today: self.today,
            };
            let (projection, outputs) = (&mut self.projection, &self.outputs);
            let overlay = self.overlay.clone();
            let dirty =
                (self.cache).update(snapshot, projection, presentation, outputs, 0, overlay);
            let entries = self.cache.entries();
            assert!(
                entries[..entries.len() - self.overlay.len()]
                    == history_entries(snapshot, projection, presentation, outputs, true),
                "the retained history differs from a fresh build"
            );
            dirty
        }
    }

    /// The entries the model's responses produced: no user input or indicator.
    fn replies(cache: &ContentCache) -> Vec<&Entry> {
        let entries = cache.entries().iter();
        let reply = |entry: &&Entry| {
            !matches!(
                entry.key(),
                EntryKey::UserBlock { .. } | EntryKey::Working(_)
            )
        };
        entries.filter(reply).collect()
    }

    /// Observe updates through the next response event.
    async fn next_response(app: &mut App) {
        while !matches!(observe_next(app).await, RuntimeEvent::ResponseEvent { .. }) {}
    }

    /// Entries carry live timings for the renderer to paint and settle to what
    /// they took; nothing unchanged rebuilds until a new local day dates earlier
    /// times.
    #[tokio::test]
    async fn entries_carry_timings_and_only_a_new_day_rebuilds_them() {
        let (_root, mut app, mut model, session) = played_fixture().await;
        let agent = app.root_agent().clone();
        let asked = ask(&session, "work");
        let turn = model.turn().await;
        let script = sleeping_script(&session);
        // The turn's attempt started before the model saw its request, so it is
        // journaled before the script's job runs.
        let script_job = |app: &App| {
            let mut jobs = app.snapshot.jobs.values();
            jobs.find(|job| job.tool == "script").map(|job| job.id)
        };
        let job = observe_find(&mut app, script_job).await;
        reached(&mut app, job, JobTransition::Running).await;
        let running = app.snapshot.clone();
        session.cancel_job(job).await.unwrap();
        let _ = bounded(script).await.unwrap();
        turn.answer("done");
        bounded(asked).await.unwrap().unwrap();
        observe_until(&mut app, |app| {
            activity(&app.snapshot, &agent) == Some(&AgentActivity::Idle)
                && recorded(&app.snapshot, completed)
        })
        .await;
        let settled = app.snapshot.clone();
        let turn = Timing::Since(running.activity[&agent].since);
        // The day the session ran, which dates nothing it shows.
        let last = settled.records.values().next_back().unwrap();
        let today = Clock::at(last.timestamp_millis).day();
        let later = today + chrono::Days::new(2);
        for tab in [Tab::Conversation, Tab::Requests, Tab::Jobs] {
            let mut fold = Fold::new(&agent, tab);
            fold.today = today;
            fold.refresh(&running);
            let counting = fold
                .cache
                .entries()
                .iter()
                .filter(|entry| entry.timing.live());
            let expected = if tab == Tab::Conversation { 2 } else { 1 };
            assert_eq!(counting.count(), expected, "{tab:?}");
            assert!(fold.refresh(&running).is_empty());
            if tab == Tab::Conversation {
                let working = fold.cache.entries().last().unwrap();
                assert_eq!((working.text(), working.timing), ("Working", turn));
            }
            fold.refresh(&settled);
            let timings: Vec<_> = (fold.cache.entries().iter())
                .map(|entry| entry.timing)
                .filter(|timing| *timing != Timing::Untimed)
                .collect();
            let took = |timing: &Timing| matches!(timing, Timing::Took { .. });
            assert!(!timings.is_empty() && timings.iter().all(took), "{tab:?}");
            // Days later, only request rows change: their send time gains its date.
            fold.today = later;
            let dated = fold.refresh(&settled);
            assert_eq!(dated.is_empty(), tab != Tab::Requests, "{tab:?}");
        }
    }

    #[tokio::test]
    async fn synchronous_failure_refreshes_the_existing_call() {
        let (_root, mut app, mut model, _session) = played_fixture().await;
        let agent = app.root_agent().clone();
        let _asked = ask(app.session().unwrap(), "test it");
        model.turn().await.end(vec![failing_call("call")]);
        observe_record(&mut app, committed(assistant)).await;
        let mut fold = Fold::new(&agent, Tab::Conversation);
        fold.refresh(&app.snapshot);
        let tool = |entry: &Entry| entry.surface == Surface::Tool;
        let card = fold.cache.entries().iter().position(tool).unwrap();
        let key = fold.cache.entries()[card].key().clone();
        assert!(!fold.cache.entries()[card].text().contains("Failed"));
        observe_record(&mut app, committed(results)).await;
        let records_before = serde_json::to_value(&app.snapshot.records).unwrap();
        assert_eq!(fold.refresh(&app.snapshot), [card]);
        let entry = &fold.cache.entries()[card];
        assert_eq!(
            (entry.key(), entry.surface, entry.job_id()),
            (&key, Surface::Tool, None)
        );
        assert_eq!(entry.text(), "▸ × exec · Failed");
        // Expanding rebuilds only the toggled entry's source.
        fold.view.set_expanded(key.clone(), true);
        fold.cache.invalidate_entry(key.clone());
        assert_eq!(fold.refresh(&app.snapshot), [card]);
        let text = fold.cache.entries()[card].text();
        assert_eq!(fold.cache.entries()[card].key(), &key);
        for part in ["Arguments", "Output"] {
            assert!(text.contains(part), "{text}");
        }
        let error = tool_error(&app.snapshot);
        assert_eq!(text.matches(error).count(), 1, "{text}");
        assert_eq!(
            records_before,
            serde_json::to_value(&app.snapshot.records).unwrap()
        );
    }

    #[tokio::test]
    async fn content_cache_refreshes_only_invalidated_tool_output() {
        let (_root, mut app) = fixture().await;
        let agent = app.root_agent().clone();
        let script = "await tool.exec({command:['true']}); await tool.exec({command:['true']});";
        app.session().unwrap().run_script(script).await.unwrap();
        observe_until(&mut app, |app| count(&app.snapshot, job_ended) == 3).await;
        let mut fold = Fold::new(&agent, Tab::Jobs);
        fold.all_details = true;
        fold.refresh(&app.snapshot);
        assert_eq!(fold.cache.entries().len(), 3);
        let exec = |entry: &Entry| {
            let tool = |job| &app.snapshot.jobs[&job].tool;
            entry.job_id().is_some_and(|job| tool(job) == "exec")
        };
        let index = fold.cache.entries().iter().position(exec).unwrap();
        let job = fold.cache.entries()[index].job_id().unwrap();
        let unchanged = fold.cache.entries().to_vec();
        let output = serde_json::json!({"result": {"stdout": "new output"}});
        loaded(&mut fold.outputs, job, output);
        fold.cache.invalidate_job(job);
        assert_eq!(fold.refresh(&app.snapshot), [index]);
        assert!(fold.cache.entries()[index].text().contains("new output"));
        let mut others = (fold.cache.entries().iter().zip(&unchanged).enumerate())
            .filter(|(position, _)| *position != index);
        assert!(others.all(|(_, (entry, before))| entry == before));
    }

    #[tokio::test]
    async fn content_cache_reasoning_matches_uncached_across_shape_changes() {
        let (_root, mut app, mut model, _session) = played_fixture().await;
        let agent = app.root_agent().clone();
        let _asked = ask(app.session().unwrap(), "think");
        let turn = model.turn().await;
        let request = observe_find(&mut app, |app| latest_request(&app.snapshot)).await;
        let mut fold = Fold::new(&agent, Tab::Conversation);
        let sync = |fold: &mut Fold, snapshot: &_| {
            fold.cache.observe_response(&agent, request);
            fold.refresh(snapshot);
        };
        sync(&mut fold, &app.snapshot);
        for text in ["\n", "first", "\n", "second", "\r\n", "third", "\n\n"] {
            turn.delta("reasoning", ItemKind::Reasoning, text);
            next_response(&mut app).await;
            sync(&mut fold, &app.snapshot);
        }
        turn.delta("text", ItemKind::Text, "answer");
        next_response(&mut app).await;
        sync(&mut fold, &app.snapshot);
        // An equal-length authoritative replacement at the end must invalidate entries.
        let provisional = app.snapshot.responses[&(agent.clone(), request)].blocks()[0]
            .text
            .clone();
        let replacement = provisional.replace("first", "FIRST");
        assert_eq!(replacement.len(), provisional.len());
        let (item, block) = (String::from("reasoning"), String::from("reasoning:0"));
        turn.complete(
            Completion::answer(vec![
                AssistantItem::reasoning("reasoning", 0, replacement, Some(replay())),
                AssistantItem::text("text", 1, "answer"),
            ])
            .unwrap(),
        );
        next_response(&mut app).await;
        sync(&mut fold, &app.snapshot);
        let reasoning = (fold.cache)
            .entries()
            .iter()
            .find(|entry| entry.surface == Surface::Reasoning);
        let reasoning = reasoning.unwrap();
        assert!(!reasoning.timing.live());
        assert_eq!(
            reasoning.title(),
            Some(&Title::disclosed("Reasoning", false))
        );
        let block_ref = |item: &str, block: &str| BlockRef {
            item: ItemId::try_from(item.to_owned()).unwrap(),
            block: BlockId::try_from(block.to_owned()).unwrap(),
        };
        let response_ref = ResponseRef::Request(request);
        let keys = vec![
            EntryKey::Reasoning {
                response: response_ref,
                block: block_ref(&item, &block),
                section: 0,
            },
            EntryKey::Block {
                response: response_ref,
                block: block_ref("text", "text:0"),
            },
        ];
        let cached = |cache: &ContentCache| {
            let replies = replies(cache).into_iter();
            replies.map(|entry| entry.key().clone()).collect::<Vec<_>>()
        };
        assert_eq!(cached(&fold.cache), keys);
        observe_record(&mut app, committed(assistant)).await;
        sync(&mut fold, &app.snapshot);
        assert_eq!(cached(&fold.cache), keys);
    }

    #[test]
    fn cached_reconnecting_indicator_tracks_activity_changes() {
        let agent = root(1);
        let mut snapshot = ObservationSnapshot::default();
        let mut fold = Fold::new(&agent, Tab::Conversation);
        let reconnecting = |attempt| AgentActivity::Reconnecting { attempt };
        for state in [
            AgentActivity::Working,
            reconnecting(2),
            reconnecting(3),
            AgentActivity::Stopped(skyhook::agent::TurnFailure::Interrupted),
        ] {
            let activity = skyhook::agent::ObservedActivity { state, since: 0 };
            let event = RuntimeEvent::Activity {
                agent: agent.clone(),
                activity,
            };
            update(&mut snapshot, event);
            fold.refresh(&snapshot);
        }
        assert!(fold.cache.entries().is_empty());
    }

    #[tokio::test]
    async fn retry_error_lifecycle_matches_fresh_rendering_and_replay() {
        let (_root, mut app, mut model, session) = played_fixture().await;
        let agent = app.root_agent().clone();
        let asked = ask(&session, "question");
        let mut fold = Fold::new(&agent, Tab::Conversation);
        let delay = std::time::Duration::from_millis(10);
        let request = observe_find(&mut app, |app| latest_request(&app.snapshot)).await;
        let started = |event: &SessionEvent| matches!(event, SessionEvent::ModelAttemptStarted(_));
        let scheduled = |event: &SessionEvent| {
            matches!(
                event,
                SessionEvent::ModelFailed {
                    failure: AttemptFailure::Retrying { .. },
                    ..
                }
            )
        };
        for attempt in 1..=3 {
            let turn = model.turn().await;
            observe_until(&mut app, |app| count(&app.snapshot, started) == attempt).await;
            fold.refresh(&app.snapshot);
            assert!(replies(&fold.cache).is_empty());
            for (index, suffix) in [format!("partial {attempt}"), " updated".into()]
                .into_iter()
                .enumerate()
            {
                turn.delta("text", ItemKind::Text, &suffix);
                next_response(&mut app).await;
                fold.cache.observe_response(&agent, request);
                let dirty = fold.refresh(&app.snapshot);
                let replies = replies(&fold.cache);
                assert_eq!(replies.len(), 1, "delta {index}");
                let reply = (fold.cache.entries().iter()).position(|entry| entry == replies[0]);
                assert!(dirty.contains(&reply.unwrap()), "delta {index}");
                assert!(!replies[0].text().contains("attempt"));
            }
            if attempt == 3 {
                let answer = vec![AssistantItem::text("answer", 0, "final answer")];
                turn.complete(Completion::answer(answer).unwrap());
                break;
            }
            let error = ProviderErrorKind::Unavailable.error("HTTP 503 [code=overloaded]");
            turn.fail(error.with_retry_after(Some(delay)));
            observe_until(&mut app, |app| count(&app.snapshot, scheduled) == attempt).await;
            fold.refresh(&app.snapshot);
            let replies = replies(&fold.cache);
            assert_eq!(replies.len(), 1);
            assert_eq!(replies[0].key(), &EntryKey::Retry(request));
            // The card counts down to the next attempt.
            let due = Timing::Until(at(&app.snapshot, scheduled, attempt - 1) + 10);
            assert_eq!(replies[0].timing, due);
            let text = replies[0].text();
            let attempt_part = format!(" · attempt {}\n", attempt + 1);
            assert!(text.starts_with("Retrying · "), "{text}");
            for part in [attempt_part.as_str(), "HTTP 503"] {
                assert!(text.contains(part), "{text}");
            }
            assert!(text.ends_with(&format!("partial {attempt} updated")));
        }
        // The attempt that succeeds commits its answer, which replaces the retry card,
        // including on replay.
        observe_record(&mut app, committed(assistant)).await;
        fold.refresh(&app.snapshot);
        let replies_now = replies(&fold.cache);
        assert_eq!(replies_now.len(), 1);
        let committed = replies_now[0].clone();
        let text = committed.text();
        assert!(
            text.ends_with("final answer")
                && !text.contains("attempt")
                && !text.contains("partial")
        );
        assert!(committed.footer.is_some());
        let _ = bounded(asked).await.unwrap();
        observe_until(&mut app, |app| count(&app.snapshot, model_failed) == 2).await;
        fold.refresh(&app.snapshot);
        // Only its footer changes, now that the request has settled.
        let intact =
            |entry: &Entry| entry.key() == committed.key() && entry.text() == committed.text();
        assert!(fold.cache.entries().iter().any(intact));
        let failures: Vec<_> = (fold.cache)
            .entries()
            .iter()
            .filter(|card| card.key() == &EntryKey::Retry(request))
            .map(Entry::text)
            .collect();
        assert!(failures.is_empty(), "{failures:?}");
        let mut replay = ObservationSnapshot::default();
        for event in app.snapshot.records.values() {
            update(&mut replay, RuntimeEvent::Record(Box::new(event.clone())));
        }
        replay.activity.clone_from(&app.snapshot.activity);
        let mut replayed = Fold::new(&agent, Tab::Conversation);
        replayed.refresh(&replay);
        assert!(fold.cache.entries() == replayed.cache.entries());
    }

    /// Every tab's retained history matches a fresh build after each update of a
    /// mixed session, and conversation updates never relay out its first entry.
    #[tokio::test]
    async fn incremental_history_matches_a_fresh_build_update_by_update() {
        let (_root, mut app, mut model, session) = played_fixture().await;
        let agent = app.root_agent().clone();
        let mut tabs =
            [Tab::Conversation, Tab::Requests, Tab::Jobs].map(|tab| Fold::new(&agent, tab));
        // Fold updates until `done`, checking every tab after each.
        let mut advance = async |app: &mut App, done: &dyn Fn(&ObservationSnapshot) -> bool| {
            while !done(&app.snapshot) {
                let event = observe_next(app).await;
                for fold in &mut tabs {
                    fold.cache.observe(&event);
                    let first = fold.cache.entries().first().cloned();
                    let dirty = fold.refresh(&app.snapshot);
                    let history = |entry: &Entry| !matches!(entry.key(), EntryKey::Working(_));
                    if fold.tab == Tab::Conversation && first.as_ref().is_some_and(history) {
                        assert!(!dirty.contains(&0), "{event:?}: {dirty:?}");
                    }
                }
            }
        };
        let asked = ask(&session, "question");
        let turn = model.turn().await;
        turn.delta("text", ItemKind::Text, "partial");
        let delay = Some(std::time::Duration::from_millis(10));
        turn.fail(
            ProviderErrorKind::Unavailable
                .error("HTTP 503")
                .with_retry_after(delay),
        );
        let turn = model.turn().await;
        let script = |children: usize| {
            let exec = "await tool.exec({command:['true']});";
            json!({"source": exec.repeat(children)})
        };
        turn.end(vec![
            AssistantItem::text("text", 0, "working on it"),
            call("admitted", "exec", json!({"command": ["true"]}), 1),
            call("script", "script", script(2), 2),
            call("parallel", "script", script(1), 3),
            call(
                "plain",
                "exec",
                json!({"command": ["sleep", "60"], "bg": true}),
                4,
            ),
        ]);
        // The background call ends only once the next request has gone out, so the
        // wait wakes on it and its event commits as a notification.
        model
            .turn()
            .await
            .end(vec![call("wait", "wait", json!({}), 0)]);
        let plain = job_of_call(&session.observe().snapshot, "plain").unwrap();
        session.cancel_job(plain).await.unwrap();
        let turn = model.turn().await;
        let notified = |snapshot: &ObservationSnapshot| recorded(snapshot, committed(job_events));
        advance(&mut app, &notified).await;
        turn.delta("text", ItemKind::Text, "cut short");
        advance(&mut app, &|snapshot| streamed(snapshot, "cut short")).await;
        session.interrupt().await;
        let _ = bounded(asked).await.unwrap();
        advance(&mut app, &|snapshot| stopped(snapshot, &agent)).await;
        session
            .record_status(agent.clone(), "done".into())
            .await
            .unwrap();
        advance(&mut app, &|snapshot| recorded(snapshot, status)).await;

        // The scripts run in parallel, so their children may interleave in the
        // journal. Whatever order they landed in, each script's children sit
        // directly under it, indented, on both tabs, and the response's tool cards
        // stay one tight block.
        let mut tree = Vec::new();
        for id in ["admitted", "script", "parallel", "plain"] {
            let job = job_of_call(&app.snapshot, id).unwrap();
            tree.push((job, 0));
            let records = app.snapshot.records.values();
            tree.extend(records.filter_map(|record| match &record.event {
                SessionEvent::JobCreated {
                    job: child,
                    parent: Some(parent),
                    ..
                } if *parent == job => Some((*child, 2)),
                _ => None,
            }));
        }
        assert_eq!(tree.len(), 7);
        let cards = |entries: &[Entry]| -> Vec<_> {
            let cards = entries.iter().filter_map(|e| Some((e.job_id()?, e.indent)));
            cards.collect()
        };
        let entries = tabs[0].cache.entries();
        let first = entries.iter().position(|entry| entry.job_id().is_some());
        let block = &entries[first.unwrap()..][..7];
        assert_eq!(cards(block), tree);
        assert!(block[..6].iter().all(|entry| entry.compact_after));
        assert_eq!(cards(tabs[2].cache.entries())[..7], tree);
    }

    #[tokio::test]
    async fn retained_owner_separates_overlay_and_keeps_replacement_indices_valid() {
        let (_root, mut app) = fixture().await;
        let agent = app.root_agent().clone();
        let mut fold = Fold::new(&agent, Tab::Conversation);
        let unsaved = |text: &str| {
            vec![Entry::new(
                EntryKey::UnsavedStatus(0),
                text.into(),
                Surface::Status,
            )]
        };
        fold.overlay = unsaved("first");
        assert_eq!(fold.refresh(&app.snapshot), [0]);
        assert_eq!(fold.cache.entries().len(), 1);
        fold.overlay = unsaved("other");
        assert_eq!(fold.refresh(&app.snapshot), [0]);
        assert_eq!(fold.cache.entries()[0].text(), "other");
        (app.session().unwrap())
            .record_status(agent.clone(), "saved".into())
            .await
            .unwrap();
        observe_record(&mut app, status).await;
        fold.refresh(&app.snapshot);
        assert_eq!(fold.cache.entries().len(), 2);
        assert_eq!(fold.cache.entries()[1].key(), &EntryKey::UnsavedStatus(0));
        fold.overlay.clear();
        fold.refresh(&app.snapshot);
        assert_eq!(fold.cache.entries().len(), 1);
        // Switching to an empty tab invalidates retained history and old overlay.
        fold.tab = Tab::Requests;
        fold.refresh(&app.snapshot);
        assert!(fold.cache.entries().is_empty());
    }
}

use super::changes::Changes;
use super::entries::{Dep, History, Inputs};
use super::live::{live_tail, live_tail_responses};
use super::{Entry, EntryKey, EntryView, Projection, Tab, UnsavedStatus, Window};
use crate::tui::app::OutputStore;
use skyhook::agent::{ObservationSnapshot, RuntimeEvent};
use skyhook::identity::{AgentId, JobId};
use skyhook::session::RequestSeq;
use std::collections::HashSet;

/// Retains rendered journal content across journal records and streaming events.
/// History folds new records and rebuilds only the entries whose sources changed;
/// the live tail and working indicator are rebuilt on each update. The caller
/// invalidates it for presentation changes, which rebuild everything, as do a
/// new agent, tab or detail setting, and a new day, since earlier times then show
/// their date; a toggled entry rebuilds only its source.
///
/// Entries and all indices share one owner. Render/export borrow the entries;
/// historical strings and documents are never cloned at the update boundary.
#[derive(Default)]
pub struct ContentCache {
    entries: Vec<Entry>,
    identity: Option<Identity>,
    history: History,
    /// Requests whose responses form the live tail.
    live: Vec<RequestSeq>,
    /// The built agent's requests whose responses changed since.
    dirty_responses: HashSet<RequestSeq>,
    invalid: HashSet<Dep>,
    /// What changed since the renderer last took the changes.
    changes: Changes,
}

/// What every entry was built for; any change rebuilds them all.
#[derive(PartialEq)]
struct Identity {
    window: u64,
    agent: AgentId,
    tab: Tab,
    all_details: bool,
    today: chrono::NaiveDate,
}

impl ContentCache {
    /// Note the responses `event` changes: response events can replace content,
    /// reorder blocks, or end a block without changing its text. Never infer
    /// cache validity from string lengths.
    /// Another agent's are built afresh when it shows.
    pub fn observe(&mut self, event: &RuntimeEvent) {
        if let RuntimeEvent::ResponseEvent { agent, request, .. }
        | RuntimeEvent::ResponseSettled { agent, request, .. } = event
            && self
                .identity
                .as_ref()
                .is_some_and(|built| &built.agent == agent)
        {
            self.dirty_responses.insert(*request);
        }
    }

    /// Refresh downloaded output for one tool without invalidating history.
    pub fn invalidate_job(&mut self, job: JobId) {
        self.invalid.insert(Dep::Job(job));
    }

    /// Rebuild every entry on the next update.
    pub fn invalidate(&mut self) {
        self.identity = None;
    }

    /// Rebuild the entry after its expansion changed.
    pub fn invalidate_entry(&mut self, key: EntryKey) {
        self.invalid.insert(Dep::Entry(key));
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The history the entries were folded from, which places them.
    pub fn history(&self) -> &History {
        &self.history
    }

    /// How the entries changed since the last call.
    pub fn take_changes(&mut self) -> Changes {
        std::mem::take(&mut self.changes)
    }

    /// Bring the entries up to date with `window`, noting what changed for the renderer.
    pub fn update(
        &mut self,
        live: &ObservationSnapshot,
        window: &mut Window,
        projection: &mut Projection,
        presentation: EntryView<'_>,
        outputs: &OutputStore,
        unsaved: &[UnsavedStatus],
    ) {
        let entries = &mut self.entries;
        let mut changed = projection.take_changes();
        changed.extend(window.take_changes());
        let (projection, window) = (&*projection, &*window);
        let EntryView {
            agent,
            tab,
            all_details,
            today,
            ..
        } = presentation;
        let identity = Identity {
            window: window.id,
            agent: agent.clone(),
            tab,
            all_details,
            today,
        };
        let dirty_responses = std::mem::take(&mut self.dirty_responses);
        changed.extend(dirty_responses.iter().copied().map(Dep::Request));
        changed.extend(self.invalid.drain());
        let inputs = Inputs {
            live,
            window,
            projection,
            presentation,
            outputs,
            unsaved,
        };
        let rebuild = self.identity.as_ref() != Some(&identity);
        let old = if rebuild {
            self.identity = Some(identity);
            let old = std::mem::take(entries);
            (self.history, *entries) = History::build(inputs);
            old
        } else {
            let tail = entries.split_off(self.history.len());
            (self.history).update(inputs, entries, changed, &mut self.changes);
            tail
        };
        if tab == Tab::Conversation {
            // Native events can replace equal-length text, reorder items, or end
            // blocks: the live suffix is rebuilt whole, never journal entries.
            let requests: Vec<_> = if rebuild {
                live_tail_responses(inputs, agent)
            } else {
                let mut requests: Vec<_> =
                    self.live.iter().copied().chain(dirty_responses).collect();
                requests.sort_unstable();
                requests.dedup();
                requests
            };
            self.live = live_tail(inputs, requests, entries);
        }
        // A rebuild keeps the layout of entries unchanged at their positions; a
        // reply committing to history takes the place of its live entries.
        let offset = if rebuild { 0 } else { self.history.len() };
        (self.changes).replace(&old, &entries[offset..], offset);
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{loaded, replay};
    use super::super::{Clock, EntryKey, ResponseRef, Surface, Timing, Title};
    use super::*;
    use crate::tests::bounded;
    use crate::tui::app::{
        App,
        played::{
            activity, ask, assistant, at, call, committed, completed, count, failing_call,
            job_ended, job_events, job_of_call, latest_request, model_failed, observe_find,
            observe_next, observe_record, observe_until, played_fixture, reached, recorded,
            results, sleeping_script, status, stopped, streamed,
        },
        tests::{fixture, fresh, journal, unsaved_at, viewing, whole_entries},
    };
    use serde_json::json;
    use skyhook::agent::{AgentActivity, RuntimeEvent, SessionHandle};
    use skyhook::job::JobTransition;
    use skyhook::provider::ProviderErrorKind;
    use skyhook::provider::protocol::{
        AssistantItem, BlockId, BlockRef, Completion, ItemId, ItemKind,
    };
    use skyhook::session::{AttemptFailure, SessionEvent};

    /// An app viewing one tab, and what a renderer following its cache's changes
    /// laid out of it.
    struct Fold {
        _root: tempfile::TempDir,
        app: App,
        laid: Vec<Option<Entry>>,
    }

    impl Fold {
        async fn new(session: &SessionHandle, tab: Tab) -> Self {
            let (root, app) = viewing(session, tab).await;
            let laid = app.entries().iter().cloned().map(Some).collect();
            Self {
                _root: root,
                app,
                laid,
            }
        }

        /// Bring the app's entries up to date and check them: equal to a fresh build
        /// from what it holds, and reproduced by moving what the changes move and
        /// laying out what they name over what was laid out before.
        fn refresh(&mut self) -> Changes {
            self.app.content_dirty = true;
            self.app.rebuild_content();
            assert!(
                self.app.entries() == fresh(&self.app),
                "the retained history differs from a fresh build"
            );
            let changes = self.app.content_cache.take_changes();
            for splice in &changes.splices {
                let range = splice.at..splice.at + splice.removed;
                (self.laid).splice(range, std::iter::repeat_n(None, splice.inserted));
            }
            for &index in &changes.dirty {
                self.laid[index] = Some(self.app.entries()[index].clone());
            }
            let laid = self.laid.iter().map(Option::as_ref);
            assert!(laid.eq(self.app.entries().iter().map(Some)), "{changes:?}");
            changes
        }

        /// Fold updates until the app has seen `revision`, checking after each.
        async fn catch_up(&mut self, revision: u64) {
            while self.app.snapshot.revision < revision {
                observe_next(&mut self.app).await;
                self.refresh();
            }
        }
    }

    /// The entries the model's responses produced: no user input or indicator.
    fn replies(app: &App) -> Vec<&Entry> {
        let entries = app.entries().iter();
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
        let mut folds = Vec::new();
        for tab in [Tab::Conversation, Tab::Requests, Tab::Jobs] {
            folds.push(Fold::new(&session, tab).await);
        }
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
        let since = app.snapshot.activity[&agent].since;
        for fold in &mut folds {
            fold.catch_up(app.snapshot.revision).await;
            let tab = fold.app.tab;
            let counting = fold
                .app
                .entries()
                .iter()
                .filter(|entry| entry.timing.live());
            let expected = if tab == Tab::Conversation { 2 } else { 1 };
            assert_eq!(counting.count(), expected, "{tab:?}");
            assert!(fold.refresh().is_empty());
            if tab == Tab::Conversation {
                let working = fold.app.entries().last().unwrap();
                assert_eq!(
                    (working.text(), working.timing),
                    ("Working", Timing::Since(since))
                );
            }
        }
        session.cancel_job(job).await.unwrap();
        let _ = bounded(script).await.unwrap();
        turn.answer("done");
        bounded(asked).await.unwrap().unwrap();
        observe_until(&mut app, |app| {
            activity(&app.snapshot, &agent) == Some(&AgentActivity::Idle)
                && recorded(&app.journal, completed)
        })
        .await;
        // The day the session ran, which dates nothing it shows.
        let last = app.journal.last().unwrap().timestamp_millis;
        for fold in &mut folds {
            fold.app.clock = Clock::at(last);
            fold.catch_up(app.snapshot.revision).await;
            let tab = fold.app.tab;
            fold.refresh();
            let timings: Vec<_> = (fold.app.entries().iter())
                .map(|entry| entry.timing)
                .filter(|timing| *timing != Timing::Untimed)
                .collect();
            let took = |timing: &Timing| matches!(timing, Timing::Took { .. });
            assert!(!timings.is_empty() && timings.iter().all(took), "{tab:?}");
            // Days later, only request rows change: their send time gains its date.
            fold.app.clock = Clock::at(last + 2 * 86_400_000);
            let dated = fold.refresh();
            assert_eq!(dated.is_empty(), tab != Tab::Requests, "{tab:?}");
        }
    }

    #[tokio::test]
    async fn synchronous_failure_refreshes_the_existing_call() {
        let (_root, app, mut model, session) = played_fixture().await;
        let mut fold = Fold::new(&session, Tab::Conversation).await;
        let _asked = ask(app.session().unwrap(), "test it");
        model.turn().await.end(vec![failing_call("call")]);
        observe_record(&mut fold.app, committed(assistant)).await;
        fold.refresh();
        let tool = |entry: &Entry| entry.surface == Surface::Tool;
        let card = fold.app.entries().iter().position(tool).unwrap();
        let key = fold.app.entries()[card].key().clone();
        // The results commit refreshes the card in place, as does expanding it.
        observe_record(&mut fold.app, committed(results)).await;
        assert_eq!(fold.refresh().dirty, [card]);
        assert_eq!(fold.app.entries()[card].key(), &key);
        fold.app.view().set_expanded(key.clone(), true);
        fold.app.content_cache.invalidate_entry(key.clone());
        assert_eq!(fold.refresh().dirty, [card]);
        assert_eq!(fold.app.entries()[card].key(), &key);
        drop(app);
    }

    #[tokio::test]
    async fn content_cache_refreshes_only_invalidated_tool_output() {
        let (_root, app) = fixture().await;
        let session = app.session().unwrap().clone();
        let mut fold = Fold::new(&session, Tab::Jobs).await;
        fold.app.details = true;
        let script = "await tool.exec({command:['true']}); await tool.exec({command:['true']});";
        session.run_script(script).await.unwrap();
        observe_until(&mut fold.app, |app| count(&app.journal, job_ended) == 3).await;
        fold.refresh();
        assert_eq!(fold.app.entries().len(), 3);
        let exec = |entry: &Entry| {
            let tool = |job| &fold.app.snapshot.jobs[&job].tool;
            entry.job_id().is_some_and(|job| tool(job) == "exec")
        };
        let index = fold.app.entries().iter().position(exec).unwrap();
        let job = fold.app.entries()[index].job_id().unwrap();
        let unchanged = fold.app.entries().to_vec();
        let output = serde_json::json!({"result": {"stdout": "new output"}});
        loaded(&mut fold.app.outputs, job, output);
        fold.app.content_cache.invalidate_job(job);
        assert_eq!(fold.refresh().dirty, [index]);
        assert!(fold.app.entries()[index].text().contains("new output"));
        let entries = fold.app.entries().iter();
        let mut others =
            (entries.zip(&unchanged).enumerate()).filter(|(position, _)| *position != index);
        assert!(others.all(|(_, (entry, before))| entry == before));
    }

    #[tokio::test]
    async fn content_cache_reasoning_matches_uncached_across_shape_changes() {
        let (_root, app, mut model, session) = played_fixture().await;
        drop(app);
        let mut fold = Fold::new(&session, Tab::Conversation).await;
        let agent = fold.app.root_agent().clone();
        let _asked = ask(&session, "think");
        let turn = model.turn().await;
        let request = observe_find(&mut fold.app, |app| latest_request(&app.journal)).await;
        fold.refresh();
        for text in ["\n", "first", "\n", "second", "\r\n", "third", "\n\n"] {
            turn.delta("reasoning", ItemKind::Reasoning, text);
            next_response(&mut fold.app).await;
            fold.refresh();
        }
        turn.delta("text", ItemKind::Text, "answer");
        next_response(&mut fold.app).await;
        fold.refresh();
        // An equal-length authoritative replacement at the end must invalidate entries.
        let response = &fold.app.snapshot.views.responses[&(agent.clone(), request)];
        let provisional = response.blocks()[0].text.clone();
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
        next_response(&mut fold.app).await;
        fold.refresh();
        let reasoning =
            (fold.app.entries().iter()).find(|entry| entry.surface == Surface::Reasoning);
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
        let cached = |app: &App| {
            let replies = replies(app).into_iter();
            replies.map(|entry| entry.key().clone()).collect::<Vec<_>>()
        };
        assert_eq!(cached(&fold.app), keys);
        observe_record(&mut fold.app, committed(assistant)).await;
        fold.refresh();
        assert_eq!(cached(&fold.app), keys);
    }

    #[tokio::test]
    async fn retry_error_lifecycle_matches_fresh_rendering_and_replay() {
        let (_root, app, mut model, session) = played_fixture().await;
        drop(app);
        let mut fold = Fold::new(&session, Tab::Conversation).await;
        // Details disclose the pending retry's whole card.
        fold.app.details = true;
        let asked = ask(&session, "question");
        let delay = std::time::Duration::from_millis(10);
        let request = observe_find(&mut fold.app, |app| latest_request(&app.journal)).await;
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
            observe_until(&mut fold.app, |app| count(&app.journal, started) == attempt).await;
            fold.refresh();
            assert!(replies(&fold.app).is_empty());
            for (index, suffix) in [format!("partial {attempt}"), " updated".into()]
                .into_iter()
                .enumerate()
            {
                turn.delta("text", ItemKind::Text, &suffix);
                next_response(&mut fold.app).await;
                let dirty = fold.refresh().dirty;
                let replies = replies(&fold.app);
                assert_eq!(replies.len(), 1, "delta {index}");
                let reply = (fold.app.entries().iter()).position(|entry| entry == replies[0]);
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
            observe_until(&mut fold.app, |app| {
                count(&app.journal, scheduled) == attempt
            })
            .await;
            fold.refresh();
            let replies = replies(&fold.app);
            assert_eq!(replies.len(), 1);
            assert_eq!(replies[0].key(), &EntryKey::Retry(request));
            // The card counts down to the next attempt.
            let due = Timing::Until(at(&fold.app.journal, scheduled, attempt - 1) + 10);
            assert_eq!(replies[0].timing, due);
            let text = replies[0].text();
            let attempt_part = format!(" · attempt {} · Unavailable\n", attempt + 1);
            assert!(text.starts_with("▾ Retrying · "), "{text}");
            for part in [attempt_part.as_str(), "HTTP 503"] {
                assert!(text.contains(part), "{text}");
            }
            assert!(text.ends_with(&format!("partial {attempt} updated")));
        }
        // The attempt that succeeds commits its answer, which replaces the retry card,
        // including on replay.
        observe_record(&mut fold.app, committed(assistant)).await;
        fold.refresh();
        let replies_now = replies(&fold.app);
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
        observe_until(&mut fold.app, |app| count(&app.journal, model_failed) == 2).await;
        fold.refresh();
        // Only its footer changes, now that the request has settled.
        let intact =
            |entry: &Entry| entry.key() == committed.key() && entry.text() == committed.text();
        assert!(fold.app.entries().iter().any(intact));
        let failures: Vec<_> = (fold.app.entries().iter())
            .filter(|card| card.key() == &EntryKey::Retry(request))
            .map(Entry::text)
            .collect();
        assert!(failures.is_empty(), "{failures:?}");
        // What the store reads of the journal shows the same.
        let read = whole_entries(&mut fold.app, true).await;
        fold.refresh();
        assert!(fold.app.entries() == read);
    }

    /// Every tab's retained history matches a fresh build after each update of a
    /// mixed session, and conversation updates never relay out its first entry.
    #[tokio::test]
    async fn incremental_history_matches_a_fresh_build_update_by_update() {
        let (_root, mut app, mut model, session) = played_fixture().await;
        let agent = app.root_agent().clone();
        let mut tabs = Vec::new();
        for tab in [Tab::Conversation, Tab::Requests, Tab::Jobs] {
            tabs.push(Fold::new(&session, tab).await);
        }
        // Fold updates until `done`, checking every tab after each.
        let mut advance = async |app: &mut App, done: &dyn Fn(&App) -> bool| {
            while !done(app) {
                observe_next(app).await;
                for fold in &mut tabs {
                    while fold.app.snapshot.revision < app.snapshot.revision {
                        let first = fold.app.entries().first().cloned();
                        let event = observe_next(&mut fold.app).await;
                        let dirty = fold.refresh().dirty;
                        let history = |entry: &Entry| !matches!(entry.key(), EntryKey::Working(_));
                        if fold.app.tab == Tab::Conversation && first.as_ref().is_some_and(history)
                        {
                            assert!(!dirty.contains(&0), "{event:?}: {dirty:?}");
                        }
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
        let plain = job_of_call(&journal(&session).await, "plain").unwrap();
        session.cancel_job(plain).await.unwrap();
        let turn = model.turn().await;
        let notified = |app: &App| recorded(&app.journal, committed(job_events));
        advance(&mut app, &notified).await;
        turn.delta("text", ItemKind::Text, "cut short");
        advance(&mut app, &|app| streamed(&app.snapshot, "cut short")).await;
        session.interrupt().await;
        let _ = bounded(asked).await.unwrap();
        advance(&mut app, &|app| stopped(&app.snapshot, &agent)).await;
        session
            .record_status(agent.clone(), "done".into())
            .await
            .unwrap();
        advance(&mut app, &|app| recorded(&app.journal, status)).await;

        // The scripts run in parallel, so their children may interleave in the
        // journal. Whatever order they landed in, each script's children sit
        // directly under it, indented, on both tabs, and the response's tool cards
        // stay one tight block.
        let mut tree = Vec::new();
        for id in ["admitted", "script", "parallel", "plain"] {
            let job = job_of_call(&app.journal, id).unwrap();
            tree.push((job, 0));
            tree.extend(app.journal.iter().filter_map(|record| match &record.event {
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
        let entries = tabs[0].app.entries();
        let first = entries.iter().position(|entry| entry.job_id().is_some());
        let block = &entries[first.unwrap()..][..7];
        assert_eq!(cards(block), tree);
        assert!(block[..6].iter().all(|entry| entry.compact_after));
        assert_eq!(cards(tabs[2].app.entries())[..7], tree);
    }

    /// Unsaved statuses keep their place as history grows: before every entry,
    /// or after the one that was newest when they arrived.
    #[tokio::test]
    async fn unsaved_statuses_keep_their_place_as_history_grows() {
        let (_root, app) = fixture().await;
        let session = app.session().unwrap().clone();
        let mut fold = Fold::new(&session, Tab::Conversation).await;
        let agent = fold.app.root_agent().clone();
        session
            .record_status(agent.clone(), "saved".into())
            .await
            .unwrap();
        observe_record(&mut fold.app, status).await;
        fold.refresh();
        let saved = fold.app.snapshot.views.entries[&agent] - 1;
        unsaved_at(&mut fold.app, Some(saved), "anchored");
        unsaved_at(&mut fold.app, None, "drafted");
        fold.refresh();
        session
            .record_status(agent.clone(), "later".into())
            .await
            .unwrap();
        let later = |event: &SessionEvent| matches!(event, SessionEvent::Status { message } if message == "later");
        observe_record(&mut fold.app, later).await;
        fold.refresh();
        let texts: Vec<_> = fold.app.entries().iter().map(Entry::text).collect();
        let statuses = ["drafted", "saved", "anchored", "later"].map(|s| format!("Status · {s}"));
        assert_eq!(texts, statuses);
    }
}

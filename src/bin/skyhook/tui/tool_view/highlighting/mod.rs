//! Bounded asynchronous syntax cache and immutable source identities.
mod syntax;
use super::super::app::Work;
use super::{Document, MAX_LINE, MAX_SECTION, Section};
use indexmap::IndexSet;
use ratatui::text::Line;
use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
    ops::Deref,
    sync::{Arc, mpsc},
    thread,
};
pub use syntax::highlight_code;
use syntax::syntax_resources;

const CACHE_SECTIONS: usize = 128;
// Source-byte admission budget, not allocated token/span output.
const CACHE_BYTES: usize = 8 * 1024 * 1024;

/// Immutable source with content metadata computed once, not on each UI cache lookup.
/// Clones share their allocation; independently rebuilt documents still reuse highlights.
#[derive(Clone, Debug)]
pub struct CodeSource {
    text: Arc<str>,
    digest: u64,
    eligible: bool,
}
impl From<&str> for CodeSource {
    fn from(text: &str) -> Self {
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        Self {
            text: Arc::from(text),
            digest: hasher.finish(),
            eligible: text.len() <= MAX_SECTION
                && text.split('\n').all(|line| line.len() <= MAX_LINE),
        }
    }
}
impl Deref for CodeSource {
    type Target = str;
    fn deref(&self) -> &str {
        &self.text
    }
}
impl AsRef<str> for CodeSource {
    fn as_ref(&self) -> &str {
        &self.text
    }
}
impl PartialEq for CodeSource {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.text, &other.text)
            || (self.digest == other.digest && self.text == other.text)
    }
}
impl Eq for CodeSource {}
impl Hash for CodeSource {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.digest.hash(state);
    }
}

/// One admission predicate for scheduling and reverse invalidation.
fn admitted(source: &CodeSource, language: &str) -> bool {
    source.eligible && !language.is_empty()
}
impl Document {
    fn admitted_codes(&self) -> impl Iterator<Item = (&CodeSource, &str)> {
        self.sections.iter().filter_map(|section| match section {
            Section::Code {
                source, language, ..
            } if admitted(source, language) => Some((source, language.as_str())),
            _ => None,
        })
    }
    /// Content IDs for indexing highlight completions back to affected documents.
    /// A digest collision only causes an extra invalidation; cache equality verifies text.
    pub fn highlight_sources(&self) -> impl Iterator<Item = u64> + '_ {
        self.admitted_codes().map(|(source, _)| source.digest)
    }
    fn keys(&self) -> impl Iterator<Item = CodeKey> + '_ {
        self.admitted_codes()
            .map(|(source, language)| CodeKey::admit(source, language).unwrap())
    }
}
/// Cloning shares an already-checked immutable source; it does not grant a cache
/// slot. Every insertion still passes the independent source-byte budget.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct CodeKey {
    source: CodeSource,
    language: String,
}
impl CodeKey {
    pub(super) fn admit(source: &CodeSource, language: &str) -> Option<Self> {
        admitted(source, language).then(|| Self {
            source: source.clone(),
            language: language.into(),
        })
    }
}
/// Styled lines, or `None` when the source falls back to plain rendering.
type HighlightResult = Option<Vec<Line<'static>>>;
enum HighlightState {
    Unscheduled,
    InFlight,
    Ready(HighlightResult),
}
struct CachedHighlight {
    last_used: u64,
    state: HighlightState,
}
struct Completion {
    generation: u64,
    key: CodeKey,
    result: HighlightResult,
}
/// A bounded worker queue keeps regex work out of the terminal/event loop.
/// Recent sections survive collapse within a bounded cache; old-session results are discarded.
pub struct HighlightCache {
    entries: HashMap<CodeKey, CachedHighlight>,
    /// The sections the latest frame wants, most wanted first.
    working_set: IndexSet<CodeKey>,
    sender: Option<mpsc::SyncSender<(u64, CodeKey)>>,
    receiver: mpsc::Receiver<Completion>,
    generation: u64,
    clock: u64,
    changed_sources: Vec<u64>,
}
#[cfg(test)]
impl Default for HighlightCache {
    fn default() -> Self {
        Self::with_notify(tokio::sync::mpsc::unbounded_channel().0)
    }
}
impl HighlightCache {
    pub fn with_notify(notify: tokio::sync::mpsc::UnboundedSender<Work>) -> Self {
        let (sender, jobs) = mpsc::sync_channel::<(u64, CodeKey)>(16);
        let (completed, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("tui-highlight".into())
            .spawn(move || {
                // Load grammars while the initial UI is being displayed, not on the first click.
                let _ = syntax_resources();
                while let Ok((generation, key)) = jobs.recv() {
                    let result = highlight_code(&key.source, &key.language);
                    if completed
                        .send(Completion {
                            generation,
                            key,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                    let _ = notify.send(Work::HighlightsReady);
                }
            });
        Self {
            entries: HashMap::new(),
            working_set: IndexSet::new(),
            sender: worker.ok().map(|_| sender),
            receiver,
            generation: 0,
            clock: 0,
            changed_sources: Vec::new(),
        }
    }
    pub fn clear(&mut self) {
        self.entries.clear();
        self.working_set.clear();
        self.changed_sources.clear();
        self.generation += 1;
    }
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        loop {
            let completion = match self.receiver.try_recv() {
                Ok(completion) => completion,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.disconnect();
                    break;
                }
            };
            if completion.generation == self.generation
                && let Some(entry) = self.entries.get_mut(&completion.key)
                && matches!(entry.state, HighlightState::InFlight)
            {
                self.changed_sources.push(completion.key.source.digest);
                entry.state = HighlightState::Ready(completion.result);
                changed = true;
            }
        }
        // In-flight entries from an old working set cannot be evicted.
        // Their completions may release the budget for desired keys which were
        // never admitted by prepare. Retry those keys without requiring another
        // document invalidation; the selected working set is itself bounded.
        if changed {
            let missing: Vec<_> = self
                .working_set
                .iter()
                .filter(|key| !self.entries.contains_key(*key))
                .cloned()
                .collect();
            self.schedule(missing.into_iter());
        }
        // Queue pressure must not strand admitted work after document preparation.
        self.submit_unscheduled();
        changed
    }
    /// Drain content IDs whose highlights completed since the last drain.
    /// Call after poll/prepare; IDs may repeat.
    pub fn take_changed_sources(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.changed_sources)
    }
    pub fn prepare<'a>(&mut self, documents: impl Iterator<Item = &'a Document>) -> bool {
        let changed = self.poll();
        // Pick a stable working set before eviction. More open sections than the budget
        // must not evict and requeue one another after every worker completion.
        self.working_set.clear();
        let mut selected = Vec::new();
        let mut bytes = 0;
        for key in documents.flat_map(|document| document.keys()) {
            if self.working_set.contains(&key) {
                continue;
            }
            if selected.len() >= CACHE_SECTIONS || bytes + key.source.len() > CACHE_BYTES {
                continue;
            }
            bytes += key.source.len();
            self.working_set.insert(key.clone());
            selected.push(key);
        }
        self.schedule(selected.into_iter());
        changed
    }
    /// Admit `keys`, then submit what the frame wants.
    fn schedule(&mut self, keys: impl Iterator<Item = CodeKey>) {
        let mut bytes: usize = self.entries.keys().map(|key| key.source.len()).sum();
        for key in keys {
            self.clock += 1;
            if !self.entries.contains_key(&key) {
                // Retain recent sections across collapse and reopening, with a fixed budget.
                while self.entries.len() >= CACHE_SECTIONS || bytes + key.source.len() > CACHE_BYTES
                {
                    let oldest = self
                        .entries
                        .iter()
                        .filter(|(key, entry)| {
                            !self.working_set.contains(*key)
                                && !matches!(entry.state, HighlightState::InFlight)
                        })
                        .min_by_key(|(_, entry)| entry.last_used)
                        .map(|(key, _)| key.clone());
                    let Some(oldest) = oldest else {
                        break;
                    };
                    bytes -= oldest.source.len();
                    self.entries.remove(&oldest);
                }
                if self.entries.len() >= CACHE_SECTIONS || bytes + key.source.len() > CACHE_BYTES {
                    continue;
                }
                bytes += key.source.len();
            }
            let entry = self.entries.entry(key.clone()).or_insert(CachedHighlight {
                state: HighlightState::Unscheduled,
                last_used: self.clock,
            });
            entry.last_used = self.clock;
        }
        self.submit_unscheduled();
    }
    /// Submit admitted sections the frame wants, most wanted first, while the
    /// worker queue has room. The cache, not its rendering callers, owns queue
    /// retry. A disconnected worker disables submission until the cache is
    /// recreated. Pending sources become evictable Unscheduled entries and keep
    /// their ordinary source-based fallback; this is deliberately not an
    /// automatic worker-restart policy.
    fn submit_unscheduled(&mut self) {
        let Some(sender) = &self.sender else {
            return;
        };
        for key in &self.working_set {
            let Some(entry) = self.entries.get_mut(key) else {
                continue;
            };
            if !matches!(entry.state, HighlightState::Unscheduled) {
                continue;
            }
            match sender.try_send((self.generation, key.clone())) {
                Ok(()) => entry.state = HighlightState::InFlight,
                Err(mpsc::TrySendError::Full(_)) => break,
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    self.disconnect();
                    break;
                }
            }
        }
    }
    fn disconnect(&mut self) {
        self.sender = None;
        for entry in self.entries.values_mut() {
            if matches!(entry.state, HighlightState::InFlight) {
                entry.state = HighlightState::Unscheduled;
            }
        }
    }
    pub(super) fn ready(&self, key: &CodeKey) -> Option<&HighlightResult> {
        match &self.entries.get(key)?.state {
            HighlightState::Ready(result) => Some(result),
            _ => None,
        }
    }
    #[cfg(test)]
    fn is_highlighted(&self, document: &Document) -> bool {
        document.keys().next().is_some() && document.keys().all(|key| self.ready(&key).is_some())
    }
    /// Block until the real worker has completed every section.
    #[cfg(test)]
    pub fn wait(&mut self, document: &Document) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while {
            self.prepare(std::iter::once(document));
            !self.is_highlighted(document)
        } {
            assert!(std::time::Instant::now() < deadline, "highlight worker");
            thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::Role;
    use super::super::tests::text;
    use super::*;
    use std::time::Duration;

    type Queue = (
        HighlightCache,
        mpsc::Receiver<(u64, CodeKey)>,
        mpsc::Sender<Completion>,
    );

    fn queued_cache(capacity: usize) -> Queue {
        let (sender, queued) = mpsc::sync_channel(capacity);
        let (completed, receiver) = mpsc::channel();
        let cache = HighlightCache {
            entries: HashMap::new(),
            working_set: IndexSet::new(),
            sender: Some(sender),
            receiver,
            generation: 0,
            clock: 0,
            changed_sources: Vec::new(),
        };
        (cache, queued, completed)
    }

    fn code(source: &str, language: &str, role: Role) -> Document {
        let mut document = Document::default();
        document.code(source, language, 0, Default::default(), role);
        document
    }

    fn complete(
        completed: &mpsc::Sender<Completion>,
        (generation, key): (u64, CodeKey),
        result: HighlightResult,
    ) {
        let completion = Completion {
            generation,
            key,
            result,
        };
        completed.send(completion).unwrap();
    }

    fn states(cache: &HighlightCache, test: impl Fn(&HighlightState) -> bool) -> usize {
        cache
            .entries
            .values()
            .filter(|entry| test(&entry.state))
            .count()
    }

    fn source_bytes(cache: &HighlightCache) -> usize {
        cache.entries.keys().map(|key| key.source.len()).sum()
    }

    #[test]
    fn styled_completion_is_not_reinterpreted_as_fallback_by_span_color() {
        let (mut cache, queued, completed) = queued_cache(1);
        let document = code("source", "js", Role::Warning);
        cache.prepare(std::iter::once(&document));
        let styled = Some(vec![Line::from("source")]);
        complete(&completed, queued.try_recv().unwrap(), styled);
        assert!(cache.poll());
        let rendered = document.lines(Some(&cache));
        assert_eq!(rendered[0].spans.last().unwrap().style.fg, None);
        assert_eq!(text(&rendered), "source");
    }

    #[test]
    fn disconnected_worker_releases_inflight_entries_without_claiming_completion() {
        let (mut cache, queued, completed) = queued_cache(1);
        let document = code("source", "js", Role::Warning);
        cache.prepare(std::iter::once(&document));
        let in_flight = |state: &_| matches!(state, HighlightState::InFlight);
        assert_eq!(states(&cache, in_flight), cache.entries.len());
        drop((queued, completed));
        assert!(!cache.poll());
        assert!(cache.sender.is_none());
        let unscheduled = |state: &_| matches!(state, HighlightState::Unscheduled);
        assert_eq!(states(&cache, unscheduled), cache.entries.len());
        assert!(!cache.is_highlighted(&document));
        assert_eq!(document.lines(Some(&cache)), document.lines(None));
    }

    #[test]
    fn unsupported_syntax_completion_preserves_each_callers_fallback_role() {
        let assert_fallback = |cache: &HighlightCache, document: &Document| {
            let tokens = |lines: Vec<Line<'_>>| {
                let spans = lines.iter().flat_map(|line| &line.spans);
                let spans = spans.filter(|span| !span.content.is_empty());
                spans
                    .map(|span| (span.content.to_string(), span.style))
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                tokens(document.lines(Some(cache))),
                tokens(document.lines(None))
            );
            assert!(cache.is_highlighted(document));
        };
        let (mut cache, queued, completed) = queued_cache(16);
        let original = "unchanged  \n\n";
        let document = code(original, "unknown-extension", Role::Constant);
        cache.prepare(std::iter::once(&document));
        let (generation, key) = queued.try_recv().unwrap();
        complete(
            &completed,
            (generation, key.clone()),
            highlight_code(&key.source, &key.language),
        );
        assert!(cache.poll());
        assert_fallback(&cache, &document);
        // The shared key must not bake in the first document's role.
        for role in [Role::Plain, Role::String, Role::Added, Role::Removed] {
            let other = code(original, "unknown-extension", role);
            assert!(!cache.prepare(std::iter::once(&other)));
            assert_fallback(&cache, &other);
        }
        for _ in 0..3 {
            assert!(!cache.prepare(std::iter::once(&document)));
            assert!(!cache.poll());
            assert!(
                queued.try_recv().is_err(),
                "unsupported syntax was requeued"
            );
        }
    }

    #[test]
    fn admitted_highlights_retry_after_queue_pressure_without_repreparing_documents() {
        let (mut cache, queued, completed) = queued_cache(1);
        let mut document = Document::default();
        for source in ["one", "two"] {
            document.code(source, "unknown", 0, Default::default(), Role::Plain);
        }
        cache.prepare(std::iter::once(&document));
        assert_eq!(
            states(&cache, |state| matches!(state, HighlightState::InFlight)),
            1
        );
        assert_eq!(
            states(&cache, |state| matches!(state, HighlightState::Unscheduled)),
            1
        );
        for _ in 0..2 {
            complete(&completed, queued.try_recv().unwrap(), None);
            assert!(cache.poll());
        }
        let fallback = |state: &_| matches!(state, HighlightState::Ready(None));
        assert_eq!(states(&cache, fallback), 2);
        assert!(!cache.prepare(std::iter::once(&document)));
        assert!(queued.try_recv().is_err());
    }

    #[test]
    fn oversized_and_unknown_source_falls_back_without_losing_text() {
        let mut cache = HighlightCache::default();
        for original in ["x".repeat(MAX_LINE + 1), "x\n".repeat(MAX_SECTION / 2 + 1)] {
            let document = code(&original, "js", Role::Plain);
            assert!(!cache.prepare(std::iter::once(&document)));
            assert!(cache.entries.is_empty());
            assert_eq!(text(&document.lines(Some(&cache))), original);
        }
        let document = code("unchanged  \n\n", "unknown-extension", Role::Plain);
        cache.wait(&document);
        assert_eq!(text(&document.lines(Some(&cache))), "unchanged  \n\n");
        // Admission and exact source identity share one boundary.
        let mut document = Document::default();
        for (source, language) in [
            ("plain".into(), ""),
            ("unknown".into(), "future-syntax"),
            ("".into(), "js"),
            ("x".repeat(MAX_LINE + 1), "js"),
            ("x\n".repeat(MAX_SECTION / 2 + 1), "js"),
        ] {
            document.code(&source, language, 0, Default::default(), Role::Plain);
        }
        let digests: Vec<_> = document.keys().map(|key| key.source.digest).collect();
        assert_eq!(digests.len(), 2);
        assert_eq!(document.highlight_sources().collect::<Vec<_>>(), digests);
        assert!(CodeKey::admit(&CodeSource::from("plain"), "").is_none());
        let long = "x".repeat(MAX_LINE + 1);
        assert!(CodeKey::admit(&CodeSource::from(long.as_str()), "js").is_none());
    }

    #[tokio::test]
    async fn worker_wakes_the_ui_and_reopening_reuses_highlights() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut cache = HighlightCache::with_notify(sender);
        let document = code("const value = 42;", "js", Role::Plain);
        cache.prepare(std::iter::once(&document));
        let wake = tokio::time::timeout(Duration::from_secs(5), receiver.recv());
        assert!(matches!(wake.await.unwrap(), Some(Work::HighlightsReady)));
        assert!(cache.poll());
        assert!(cache.is_highlighted(&document));
        cache.prepare(std::iter::empty());
        assert!(!cache.prepare(std::iter::once(&document)));
        assert!(cache.is_highlighted(&document));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn more_open_sections_than_the_cache_budget_do_not_requeue_forever() {
        let (mut cache, queued, completed) = queued_cache(512);
        let documents: Vec<_> = (0..CACHE_SECTIONS + 10)
            .map(|index| code(&format!("const value = {index};"), "js", Role::Plain))
            .collect();
        cache.prepare(documents.iter());
        assert_eq!(cache.entries.len(), CACHE_SECTIONS);
        for queued in queued.try_iter() {
            complete(&completed, queued, Some(vec![Line::from("ready")]));
        }
        assert!(cache.prepare(documents.iter()));
        for _ in 0..3 {
            assert!(!cache.prepare(documents.iter()));
            assert!(queued.try_recv().is_err());
        }
        assert!(source_bytes(&cache) <= CACHE_BYTES);
        // Switching the working set still admits a newly visible section.
        cache.prepare(std::iter::once(documents.last().unwrap()));
        assert_eq!(queued.try_iter().count(), 1);
        // Cloned admitted keys still obey the source byte budget.
        let (mut cache, _queued, _completed) = queued_cache(CACHE_SECTIONS);
        let source = "x\n".repeat(MAX_SECTION / 2);
        let key = CodeKey::admit(&CodeSource::from(source.as_str()), "js").unwrap();
        cache.schedule(std::iter::repeat_n(key.clone(), CACHE_SECTIONS + 1));
        assert_eq!(cache.entries.len(), 1);
        let keys = (0..CACHE_BYTES / source.len() + 8).map(|index| {
            let text = format!("{index}\n{}", &source[..source.len() - 8]);
            CodeKey::admit(&CodeSource::from(text.as_str()), "js").unwrap()
        });
        cache.schedule(keys);
        assert!(source_bytes(&cache) <= CACHE_BYTES);
        assert!(cache.entries.len() < CACHE_SECTIONS);
    }

    #[test]
    fn sections_reach_the_worker_in_priority_order_across_refills() {
        let (mut cache, queued, completed) = queued_cache(2);
        let documents: Vec<_> = (0..6)
            .map(|index| code(&format!("const value = {index};"), "js", Role::Plain))
            .collect();
        // Later documents are more wanted, as the visible ones are.
        cache.prepare(documents.iter().rev());
        let mut order = Vec::new();
        while order.len() < documents.len() {
            let batch: Vec<_> = queued.try_iter().collect();
            assert!(
                !batch.is_empty(),
                "the worker is refilled after each completion"
            );
            for queued in batch {
                order.push(queued.1.source.to_string());
                complete(&completed, queued, None);
            }
            cache.poll();
        }
        let expected: Vec<_> = (0..6)
            .rev()
            .map(|index| format!("const value = {index};"))
            .collect();
        assert_eq!(order, expected);
    }

    #[test]
    fn cache_discards_old_sessions_and_reuses_collapsed_sections() {
        // Real completions follow submission; retain both generations in the
        // fake queue so this exercises late results without inventing a result
        // for an Unscheduled entry.
        let (mut cache, _queued, sender) = queued_cache(2);
        let document = code("return 42;", "js", Role::Plain);
        let key = document.keys().next().unwrap();
        cache.prepare(std::iter::once(&document));
        cache.clear();
        cache.prepare(std::iter::once(&document));
        let styled = |text| Some(vec![Line::from(text)]);
        complete(&sender, (0, key.clone()), styled("stale"));
        assert!(!cache.prepare(std::iter::once(&document)));
        assert!(!cache.is_highlighted(&document));
        complete(&sender, (1, key), styled("collapsed"));
        assert!(cache.prepare(std::iter::empty()));
        assert!(cache.is_highlighted(&document));
        assert!(!cache.prepare(std::iter::once(&document)));
        assert!(cache.is_highlighted(&document));
        // A completion whose entry was evicted publishes nothing.
        let (mut cache, queued, completed) = queued_cache(2);
        let document = code("source", "js", Role::Plain);
        cache.prepare(std::iter::once(&document));
        let (generation, key) = queued.try_recv().unwrap();
        cache.entries.remove(&key);
        complete(&completed, (generation, key), None);
        assert!(!cache.poll());
        assert!(!cache.is_highlighted(&document));
        assert!(cache.take_changed_sources().is_empty());
    }
}

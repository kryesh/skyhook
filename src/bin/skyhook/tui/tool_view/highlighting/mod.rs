//! Bounded asynchronous syntax cache and immutable source identities.
mod syntax;
use super::super::app::Work;
use super::{Document, MAX_LINE, MAX_SECTION, Section, model};
use ratatui::text::Line;
use std::{
    collections::{HashMap, HashSet},
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

impl Document {
    /// Content IDs for indexing highlight completions back to affected documents.
    /// A digest collision only causes an extra invalidation; cache equality verifies text.
    pub fn highlight_sources(&self) -> impl Iterator<Item = u64> + '_ {
        self.sections.iter().filter_map(|section| match section {
            Section::Code {
                source, language, ..
            } if source.eligible && !language.is_empty() => Some(source.digest),
            _ => None,
        })
    }
    fn keys(&self) -> impl Iterator<Item = CodeKey> + '_ {
        self.sections
            .iter()
            .filter_map(move |section| match section {
                Section::Code {
                    source, language, ..
                } if source.eligible && !language.is_empty() => {
                    Some(CodeKey::new(source.clone(), language))
                }
                _ => None,
            })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct CodeKey {
    source: CodeSource,
    language: String,
}
impl CodeKey {
    pub(super) fn new(source: CodeSource, language: &str) -> Self {
        Self {
            source,
            language: language.into(),
        }
    }
}
struct CachedHighlight {
    last_used: u64,
    pending: bool,
    lines: Option<Vec<Line<'static>>>,
}
struct Completion {
    generation: u64,
    key: CodeKey,
    lines: Vec<Line<'static>>,
}
/// A bounded worker queue keeps regex work out of the terminal/event loop.
/// Recent sections survive collapse within a bounded cache; old-session results are discarded.
pub struct HighlightCache {
    entries: HashMap<CodeKey, CachedHighlight>,
    working_set: HashSet<CodeKey>,
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
                    let lines = highlight(&key);
                    if completed
                        .send(Completion {
                            generation,
                            key,
                            lines,
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
            working_set: HashSet::new(),
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
        for completion in self.receiver.try_iter() {
            if completion.generation == self.generation
                && let Some(entry) = self.entries.get_mut(&completion.key)
            {
                self.changed_sources.push(completion.key.source.digest);
                entry.lines = Some(completion.lines);
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
        // Queue pressure must not strand admitted sections when the renderer
        // subsequently prepares only changed documents. This bounded cache owns
        // retrying its unscheduled work independently of view invalidations.
        if let Some(sender) = &self.sender {
            for (key, entry) in &mut self.entries {
                if !entry.pending && entry.lines.is_none() {
                    if sender.try_send((self.generation, key.clone())).is_err() {
                        break;
                    }
                    entry.pending = true;
                }
            }
        }
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
                                && (!entry.pending || entry.lines.is_some())
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
                pending: false,
                lines: None,
                last_used: self.clock,
            });
            entry.last_used = self.clock;
            if !entry.pending
                && let Some(sender) = &self.sender
                && sender.try_send((self.generation, key)).is_ok()
            {
                entry.pending = true;
            }
        }
    }
    pub(super) fn ready(&self, key: &CodeKey) -> Option<&Vec<Line<'static>>> {
        self.entries.get(key)?.lines.as_ref()
    }
    #[cfg(test)]
    pub fn is_highlighted(&self, document: &Document) -> bool {
        document.keys().any(|key| self.ready(&key).is_some())
    }
    #[cfg(test)]
    pub fn is_fully_highlighted(&self, document: &Document) -> bool {
        document.keys().peekable().peek().is_some()
            && document.keys().all(|key| self.ready(&key).is_some())
    }
}
fn highlight(key: &CodeKey) -> Vec<Line<'static>> {
    highlight_code(&key.source, &key.language).unwrap_or_else(|| {
        key.source
            .split('\n')
            .map(|text| Line::from(model::clean(text)))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::super::Role;
    use super::*;
    use std::time::{Duration, Instant};

    fn queued_cache(
        capacity: usize,
    ) -> (
        HighlightCache,
        mpsc::Receiver<(u64, CodeKey)>,
        mpsc::Sender<Completion>,
    ) {
        let (sender, queued) = mpsc::sync_channel(capacity);
        let (completed, receiver) = mpsc::channel();
        (
            HighlightCache {
                entries: HashMap::new(),
                working_set: HashSet::new(),
                sender: Some(sender),
                receiver,
                generation: 0,
                clock: 0,
                changed_sources: Vec::new(),
            },
            queued,
            completed,
        )
    }

    #[test]
    fn unsupported_syntax_completion_preserves_each_callers_fallback_role() {
        let styled_tokens = |lines: &[Line<'_>]| {
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .filter(|span| !span.content.is_empty())
                .map(|span| (span.content.to_string(), span.style))
                .collect::<Vec<_>>()
        };
        let (mut cache, queued, completed) = queued_cache(16);
        let mut document = Document::default();
        let original = "unchanged  \n\n";
        document.code(original, "unknown-extension", 0, vec![], Role::Constant);
        let pending = document.lines(None);
        cache.prepare(std::iter::once(&document));
        let (generation, key) = queued.try_recv().unwrap();
        completed
            .send(Completion {
                generation,
                lines: highlight(&key),
                key,
            })
            .unwrap();
        assert!(cache.poll());
        assert!(cache.is_fully_highlighted(&document));
        let rendered = document.lines(Some(&cache));
        assert_eq!(text(&rendered), text(&pending));
        assert_eq!(styled_tokens(&rendered), styled_tokens(&pending));

        // The shared key must not bake in the first document's role.
        for role in [Role::Plain, Role::String, Role::Added, Role::Removed] {
            let mut other = Document::default();
            other.code(original, "unknown-extension", 0, vec![], role);
            assert!(!cache.prepare(std::iter::once(&other)));
            let rendered = other.lines(Some(&cache));
            let pending = other.lines(None);
            assert_eq!(text(&rendered), text(&pending));
            assert_eq!(styled_tokens(&rendered), styled_tokens(&pending));
            assert!(cache.is_fully_highlighted(&other));
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

    fn finish(cache: &mut HighlightCache, document: &Document) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            cache.prepare(std::iter::once(document));
            if document.keys().all(|key| cache.ready(&key).is_some()) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "highlight worker did not complete"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn admitted_highlights_retry_after_queue_pressure_without_repreparing_documents() {
        let mut cache = HighlightCache::default();
        let mut documents = Vec::new();
        for index in 0..40 {
            let mut doc = Document::default();
            doc.code(
                &format!("let value_{index} = {index};"),
                "rust",
                0,
                Vec::new(),
                Role::Plain,
            );
            documents.push(doc);
        }
        cache.prepare(documents.iter());
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < until {
            cache.poll();
            if documents.iter().all(|doc| cache.is_highlighted(doc)) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("admitted sections were stranded after the worker queue filled");
    }

    #[test]
    fn oversized_and_unknown_source_falls_back_without_losing_text() {
        let mut cache = HighlightCache::default();
        for original in ["x".repeat(MAX_LINE + 1), "x\n".repeat(MAX_SECTION / 2 + 1)] {
            let mut document = Document::default();
            document.code(&original, "js", 0, vec![], Role::Plain);
            assert!(!cache.prepare(std::iter::once(&document)));
            assert!(cache.entries.is_empty());
            assert_eq!(text(&document.lines(Some(&cache))), original);
        }
        let mut document = Document::default();
        document.code(
            "unchanged  \n\n",
            "unknown-extension",
            0,
            vec![],
            Role::Plain,
        );
        finish(&mut cache, &document);
        assert_eq!(text(&document.lines(Some(&cache))), "unchanged  \n\n");
    }

    #[tokio::test]
    async fn worker_wakes_the_ui_and_reopening_reuses_highlights() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut cache = HighlightCache::with_notify(sender);
        let mut document = Document::default();
        document.code("const value = 42;", "js", 0, vec![], Role::Plain);
        cache.prepare(std::iter::once(&document));
        let wake = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
            .await
            .unwrap();
        assert!(matches!(wake, Some(Work::HighlightsReady)));
        assert!(cache.poll());
        assert!(cache.is_highlighted(&document));
        cache.prepare(std::iter::empty());
        let ready = cache.is_highlighted(&document);
        assert!(!cache.prepare(std::iter::once(&document)));
        assert_eq!(cache.is_highlighted(&document), ready);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn more_open_sections_than_the_cache_budget_do_not_requeue_forever() {
        let (mut cache, queued, completed) = queued_cache(512);
        let documents: Vec<_> = (0..CACHE_SECTIONS + 10)
            .map(|index| {
                let mut document = Document::default();
                document.code(
                    &format!("const value = {index};"),
                    "js",
                    0,
                    vec![],
                    Role::Plain,
                );
                document
            })
            .collect();
        cache.prepare(documents.iter());
        assert_eq!(cache.entries.len(), CACHE_SECTIONS);
        for (generation, key) in queued.try_iter() {
            completed
                .send(Completion {
                    generation,
                    key,
                    lines: vec![Line::from("ready")],
                })
                .unwrap();
        }
        assert!(cache.prepare(documents.iter()));
        for _ in 0..3 {
            assert!(!cache.prepare(documents.iter()));
            assert!(queued.try_recv().is_err());
        }
        assert!(
            cache
                .entries
                .keys()
                .map(|key| key.source.len())
                .sum::<usize>()
                <= CACHE_BYTES
        );
        // Switching the working set still admits a newly visible section.
        cache.prepare(std::iter::once(documents.last().unwrap()));
        assert_eq!(queued.try_iter().count(), 1);
    }

    #[test]
    fn cache_discards_old_sessions_and_reuses_collapsed_sections() {
        let (sender, receiver) = mpsc::channel();
        let mut cache = HighlightCache {
            entries: HashMap::new(),
            working_set: HashSet::new(),
            sender: None,
            receiver,
            generation: 0,
            clock: 0,
            changed_sources: Vec::new(),
        };
        let mut document = Document::default();
        document.code("return 42;", "js", 0, vec![], Role::Plain);
        let key = document.keys().next().unwrap();
        cache.prepare(std::iter::once(&document));
        cache.clear();
        cache.prepare(std::iter::once(&document));
        sender
            .send(Completion {
                generation: 0,
                key: key.clone(),
                lines: vec![Line::from("stale")],
            })
            .unwrap();
        assert!(!cache.prepare(std::iter::once(&document)));
        assert!(!cache.is_highlighted(&document));
        sender
            .send(Completion {
                generation: 1,
                key,
                lines: vec![Line::from("collapsed")],
            })
            .unwrap();
        assert!(cache.prepare(std::iter::empty()));
        let ready = cache.is_highlighted(&document);
        assert!(ready);
        assert!(!cache.prepare(std::iter::once(&document)));
        assert_eq!(cache.is_highlighted(&document), ready);
    }
}

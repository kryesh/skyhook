//! Tab selection and ordered conversation projection, including call/result provenance.
//!
//! History is a fold over sources in place order: the loaded records of the viewed
//! agent's transcript, unsaved statuses, requests, jobs with the children loaded under
//! them, and what is not loaded past either end. Each source owns a contiguous segment of
//! entries, rebuilt only when something it was built from changes, so a new record
//! costs its own entries, not the history's.
mod build;

use crate::tui::app::OutputStore;

use super::changes::Changes;
use super::live::{live_tail, live_tail_responses};
use super::window::{Content, Window};
use super::{Entry, EntryKey, EntryView, Focus, Gap, Projection, Surface, Tab, UnsavedStatus};
use skyhook::agent::ObservationSnapshot;
use skyhook::identity::JobId;
use skyhook::job::{JobEnvelope, JobRole};
use skyhook::session::{MessageSeq, RecordSeq, RequestRecord, RequestSeq, SessionEvent};
use std::collections::{HashMap, HashSet};
use std::ops::Range;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolGroup {
    Response(MessageSeq),
    /// A job no response called, with its children.
    Job(JobId),
    Notification(RecordSeq, usize),
}

/// Something history entries are built from besides their own source.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Dep {
    /// A model request's ledger record or observed response.
    Request(RequestSeq),
    /// A job's state, output or owning agent.
    Job(JobId),
    /// A tool call's result, or the job it admitted.
    Call(MessageSeq, String),
    /// The view's expansion of the entry with this key.
    Entry(EntryKey),
}

/// What a segment is built from. Sources sort by place: transcript entries by
/// ordinal, unsaved statuses before the entries after their anchor, and a parent's
/// children after it, then its next page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Source {
    /// What lies before the first entry a list loaded.
    Earlier,
    Unsaved(usize),
    /// A transcript entry by ordinal.
    Transcript(u64),
    Request(RequestSeq),
    Job(JobId),
    /// A parent's children after those loaded.
    More(JobId),
    /// What lies after the last entry a list loaded.
    Later,
}

impl Source {
    /// What the window has not loaded at this source.
    fn gap(self) -> Option<Gap> {
        match self {
            Self::Earlier => Some(Gap::Earlier),
            Self::More(parent) => Some(Gap::More(parent)),
            Self::Later => Some(Gap::Later),
            _ => None,
        }
    }
}

/// Where a source's segment sorts: its parent's place, then its own source, so a
/// script's children sit directly under it even while scripts run in parallel.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Place(Vec<Source>);
impl Place {
    fn under(parent: Option<&Self>, source: Source) -> Self {
        let mut path = parent.map_or_else(Vec::new, |parent| parent.0.clone());
        path.push(source);
        Self(path)
    }

    fn source(&self) -> Source {
        *self.0.last().expect("a place ends with its own source")
    }

    fn depth(&self) -> usize {
        self.0.len() - 1
    }

    /// The cells a card at this place is indented by.
    fn indent(&self) -> u16 {
        (self.depth().min(MAX_NESTING) * NEST_INDENT) as u16
    }

    /// The tab's own entry this place belongs to, which anchors the view.
    fn focus(&self) -> Option<Focus> {
        match *self.0.first()? {
            Source::Transcript(ordinal) => Some(Focus::Ordinal(ordinal)),
            Source::Request(request) => Some(Focus::Request(request)),
            Source::Job(job) => Some(Focus::Job(job)),
            _ => None,
        }
    }
}

struct Segment {
    place: Place,
    start: usize,
    len: usize,
    /// What it was built from.
    deps: Vec<Dep>,
}

/// Job cards indent this many cells per level, up to `MAX_NESTING` levels.
const NEST_INDENT: usize = 2;
const MAX_NESTING: usize = 8;

/// One source's entries, their tool groups, and what they were built from.
#[derive(Default)]
struct Built {
    entries: Vec<Entry>,
    groups: Vec<Option<ToolGroup>>,
    deps: Vec<Dep>,
}
impl Built {
    fn push(&mut self, entry: Entry, group: Option<ToolGroup>) {
        self.deps.push(Dep::Entry(entry.key().clone()));
        self.entries.push(entry);
        self.groups.push(group);
    }
}

/// Everything history is built from.
#[derive(Clone, Copy)]
pub struct Inputs<'a> {
    pub live: &'a ObservationSnapshot,
    pub window: &'a Window,
    pub projection: &'a Projection,
    pub presentation: EntryView<'a>,
    pub outputs: &'a OutputStore,
    pub unsaved: &'a [UnsavedStatus],
}

impl<'a> Inputs<'a> {
    /// `request` as the session reads it: each agent's newest as the live slice holds
    /// it, which a session that stopped settles; any other as the window has it.
    pub fn request(&self, request: RequestSeq) -> Option<&'a RequestRecord> {
        (self.live.requests.get(request)).or_else(|| self.window.request(request))
    }

    /// A job as the session last showed it.
    pub fn job(&self, job: &JobId) -> Option<&'a JobEnvelope> {
        self.projection.job(self.live, Some(self.window), job)
    }

    /// Every source the window holds for the viewed tab, in place order.
    fn places(&self) -> Vec<Place> {
        let mut places = Vec::new();
        let content = &self.window.content;
        let (earlier, later) = content.ends();
        places.extend(earlier.then(|| Place::under(None, Source::Earlier)));
        places.extend(later.then(|| Place::under(None, Source::Later)));
        match content {
            Content::Conversation(transcript) => {
                for (ordinal, record) in transcript.records() {
                    let place = Place::under(None, Source::Transcript(ordinal));
                    places.push(place.clone());
                    if let SessionEvent::JobCreated { job, .. } = &record.event {
                        self.children(&place, *job, &mut places);
                    }
                }
                // A status shows where its anchor is loaded.
                let agent = self.presentation.agent;
                let unsaved = self.unsaved.iter().enumerate();
                let unsaved = unsaved.filter(|(_, status)| {
                    &status.agent == agent
                        && status
                            .after
                            .map_or(!earlier, |after| transcript.record(after).is_some())
                });
                places.extend(unsaved.map(|(index, status)| {
                    let anchor =
                        (status.after).map(|after| Place::under(None, Source::Transcript(after)));
                    Place::under(anchor.as_ref(), Source::Unsaved(index))
                }));
            }
            Content::Requests(list) => {
                let requests = list.keys.iter();
                places
                    .extend(requests.map(|request| Place::under(None, Source::Request(*request))));
            }
            Content::Jobs(list) => {
                for job in &list.keys {
                    let place = Place::under(None, Source::Job(*job));
                    places.push(place.clone());
                    self.children(&place, *job, &mut places);
                }
            }
        }
        places.sort_unstable();
        places.dedup();
        places
    }

    /// The places of `job`'s loaded children under `parent`, and theirs, then of what
    /// is left to load of them: a script's whose children were not read yet included.
    fn children(&self, parent: &Place, job: JobId, places: &mut Vec<Place>) {
        let Some(children) = self.window.children.get(&job) else {
            let script = self
                .job(&job)
                .is_some_and(|job| job.role == JobRole::Script);
            places.extend(script.then(|| Place::under(Some(parent), Source::More(job))));
            return;
        };
        for child in &children.keys {
            let place = Place::under(Some(parent), Source::Job(*child));
            places.push(place.clone());
            if place.depth() < 2 * MAX_NESTING {
                self.children(&place, *child, places);
            }
        }
        if children.later {
            places.push(Place::under(Some(parent), Source::More(job)));
        }
    }
}

/// Retained history for one agent and tab.
#[derive(Default)]
pub struct History {
    segments: Vec<Segment>,
    index: HashMap<Source, usize>,
    deps: HashMap<Dep, Vec<Source>>,
    /// Each entry's tool group, for the spacing between siblings.
    groups: Vec<Option<ToolGroup>>,
    /// The window shape and how many unsaved statuses there were when the places
    /// were last read.
    shape: Option<(u64, usize)>,
}

impl History {
    pub fn len(&self) -> usize {
        self.groups.len()
    }

    /// A fresh history and its entries.
    pub fn build(inputs: Inputs<'_>) -> (Self, Vec<Entry>) {
        let mut history = Self {
            shape: Some((inputs.window.shape, inputs.unsaved.len())),
            ..Self::default()
        };
        let mut entries = Vec::new();
        for place in inputs.places() {
            let built = build::source(&place, inputs);
            history.push(place, built, &mut entries);
        }
        let all = 0..entries.len();
        history.compact(inputs, &mut entries, all, &mut Changes::default());
        (history, entries)
    }

    /// Bring the places up to the window's and rebuild what `changed` names, reporting
    /// to `changes`. `entries` holds exactly this history's entries.
    pub fn update(
        &mut self,
        inputs: Inputs<'_>,
        entries: &mut Vec<Entry>,
        changed: HashSet<Dep>,
        changes: &mut Changes,
    ) {
        let shape = (inputs.window.shape, inputs.unsaved.len());
        let reshaped = self.shape != Some(shape);
        let mut rebuilt: Vec<_> = changed
            .iter()
            .filter_map(|dep| self.deps.get(dep))
            .flatten()
            .filter_map(|source| self.index.get(source).copied())
            .collect();
        rebuilt.sort_unstable();
        rebuilt.dedup();
        let places = reshaped.then(|| inputs.places());
        self.shape = Some(shape);
        self.apply(inputs, entries, rebuilt, places, changes);
    }

    /// Rebuild the segments at `rebuilt`, and when `places` is given, take its places:
    /// a rebuilt segment that keeps its length is replaced in place; the rest merge in
    /// one pass from the first segment that moves.
    fn apply(
        &mut self,
        inputs: Inputs<'_>,
        entries: &mut Vec<Entry>,
        rebuilt: Vec<usize>,
        places: Option<Vec<Place>>,
        changes: &mut Changes,
    ) {
        let mut touched = Vec::new();
        let mut resized = HashMap::new();
        for position in rebuilt {
            let place = self.segments[position].place.clone();
            let built = build::source(&place, inputs);
            let segment = &mut self.segments[position];
            let (start, len) = (segment.start, segment.len);
            touched.push(place.source());
            if built.entries.len() != len {
                resized.insert(position, built);
                continue;
            }
            self.register(position, built.deps);
            let old = entries[start..start + len].iter_mut();
            for (offset, (old, mut new)) in old.zip(built.entries).enumerate() {
                // Compaction below decides this from the neighbours as they end up.
                new.compact_after = old.compact_after;
                if *old != new {
                    *old = new;
                    changes.changed(start + offset);
                }
            }
            self.groups[start..start + len].copy_from_slice(&built.groups);
        }
        // The first segment whose place the window no longer holds there.
        let moved = places.as_ref().map(|places| {
            let same = self.segments.iter().zip(places);
            let same = same
                .take_while(|(segment, place)| segment.place == **place)
                .count();
            (same < self.segments.len().max(places.len())).then_some(same)
        });
        let from = resized.keys().copied().chain(moved.flatten()).min();
        if let Some(from) = from {
            let places =
                places.unwrap_or_else(|| self.segments.iter().map(|s| s.place.clone()).collect());
            let new = places[from.min(places.len())..].to_vec();
            touched.extend(new.iter().map(Place::source));
            // The entry before one removed may no longer sit tight against what follows.
            for at in self.merge(from, new, resized, inputs, entries, changes) {
                self.compact(inputs, entries, at.saturating_sub(1)..at, changes);
            }
        }
        for source in touched {
            if let Some(segment) = self.index.get(&source).map(|index| &self.segments[*index]) {
                let range = segment.start.saturating_sub(1)..segment.start + segment.len;
                self.compact(inputs, entries, range, changes);
            }
        }
    }

    /// Lay the segments from position `from` out again as `places` lists them, with
    /// `resized` rebuilt to another length, moving those kept and dropping the rest.
    /// Returns where entries were dropped.
    fn merge(
        &mut self,
        from: usize,
        places: Vec<Place>,
        mut resized: HashMap<usize, Built>,
        inputs: Inputs<'_>,
        entries: &mut Vec<Entry>,
        changes: &mut Changes,
    ) -> Vec<usize> {
        let mut dropped = Vec::new();
        let start = self.segments.get(from).map_or(entries.len(), |s| s.start);
        let mut moved_entries = entries.split_off(start).into_iter();
        let mut moved_groups = self.groups.split_off(start).into_iter();
        let old = self.segments.split_off(from);
        for segment in &old {
            self.index.remove(&segment.place.source());
        }
        let mut old = (from..).zip(old).peekable();
        let mut places = places.into_iter().peekable();
        loop {
            // Old segments before the next place are gone; one at it is kept.
            let step = match (places.peek(), old.peek()) {
                (None, None) => break dropped,
                (Some(place), Some((_, segment))) => place.cmp(&segment.place),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
            };
            let at = entries.len();
            match step {
                std::cmp::Ordering::Greater => {
                    let (_, segment) = old.next().expect("peeked");
                    moved_entries.by_ref().take(segment.len).for_each(drop);
                    moved_groups.by_ref().take(segment.len).for_each(drop);
                    self.unregister(&segment);
                    changes.splice(at, segment.len, 0);
                    dropped.push(at);
                }
                std::cmp::Ordering::Less => {
                    let place = places.next().expect("peeked");
                    let built = build::source(&place, inputs);
                    changes.splice(at, 0, built.entries.len());
                    self.push(place, built, entries);
                }
                std::cmp::Ordering::Equal => {
                    let (position, segment) = old.next().expect("peeked");
                    let place = places.next().expect("peeked");
                    let old_entries = moved_entries.by_ref().take(segment.len);
                    let old_groups = moved_groups.by_ref().take(segment.len);
                    match resized.remove(&position) {
                        Some(built) => {
                            old_entries.for_each(drop);
                            old_groups.for_each(drop);
                            self.unregister(&segment);
                            changes.splice(at, segment.len, built.entries.len());
                            self.push(place, built, entries);
                        }
                        None => {
                            entries.extend(old_entries);
                            self.groups.extend(old_groups);
                            self.index.insert(place.source(), self.segments.len());
                            self.segments.push(Segment {
                                start: at,
                                ..segment
                            });
                        }
                    }
                }
            }
        }
    }

    /// Append a segment built for `place`.
    fn push(&mut self, place: Place, built: Built, entries: &mut Vec<Entry>) {
        let (start, len) = (entries.len(), built.entries.len());
        entries.extend(built.entries);
        self.groups.extend(built.groups);
        let position = self.segments.len();
        self.index.insert(place.source(), position);
        let source = place.source();
        for dep in &built.deps {
            let sources = self.deps.entry(dep.clone()).or_default();
            if !sources.contains(&source) {
                sources.push(source);
            }
        }
        self.segments.push(Segment {
            place,
            start,
            len,
            deps: built.deps,
        });
    }

    /// Register the segment at `position` under `deps` instead of what it had.
    fn register(&mut self, position: usize, deps: Vec<Dep>) {
        let segment = std::mem::take(&mut self.segments[position].deps);
        let source = self.segments[position].place.source();
        self.forget(source, &segment);
        for dep in &deps {
            let sources = self.deps.entry(dep.clone()).or_default();
            if !sources.contains(&source) {
                sources.push(source);
            }
        }
        self.segments[position].deps = deps;
    }

    fn unregister(&mut self, segment: &Segment) {
        self.forget(segment.place.source(), &segment.deps);
    }

    fn forget(&mut self, source: Source, deps: &[Dep]) {
        for dep in deps {
            if let Some(sources) = self.deps.get_mut(dep) {
                sources.retain(|kept| *kept != source);
                if sources.is_empty() {
                    self.deps.remove(dep);
                }
            }
        }
    }

    /// An entry sits tight against the next when both share a tool group. It is
    /// stored on the entry so equality-based invalidation also relays out a
    /// neighbour when a sibling arrives or leaves.
    fn compact(
        &self,
        inputs: Inputs<'_>,
        entries: &mut [Entry],
        range: Range<usize>,
        changes: &mut Changes,
    ) {
        if inputs.presentation.tab != Tab::Conversation {
            return;
        }
        for index in range.start..range.end.min(entries.len()) {
            let next = self.groups.get(index + 1).copied().flatten();
            let compact = self.groups[index].is_some_and(|group| next == Some(group));
            if entries[index].compact_after != compact {
                entries[index].compact_after = compact;
                changes.changed(index);
            }
        }
    }

    /// The entries the loaded tab's own entry `focus` shows, with those placed under
    /// it.
    pub fn place(&self, focus: Focus) -> Option<Range<usize>> {
        let own = match focus {
            Focus::Ordinal(ordinal) => Source::Transcript(ordinal),
            Focus::Request(request) => Source::Request(request),
            Focus::Job(job) => Source::Job(job),
        };
        let position = *self.index.get(&own)?;
        let first = (self.segments.get(position)).filter(|first| first.place.depth() == 0)?;
        let under = self.segments[position + 1..].iter();
        let under = under.take_while(|segment| segment.place.0[0] == own);
        let end = under
            .last()
            .map_or(first.start + first.len, |s| s.start + s.len);
        Some(first.start..end)
    }

    /// The tab's own entry the entry at `index` belongs to.
    pub fn place_of(&self, index: usize) -> Option<Focus> {
        let at = self
            .segments
            .partition_point(|segment| segment.start + segment.len <= index);
        self.segments.get(at)?.place.focus()
    }
}

pub(super) fn status_entry(key: EntryKey, message: &dyn std::fmt::Display) -> Entry {
    Entry::new(key, format!("Status · {message}"), Surface::Status)
}

/// Shared by retained UI content and fresh export construction.
pub fn entries(inputs: Inputs<'_>, include_live: bool) -> Vec<Entry> {
    let (_, mut entries) = History::build(inputs);
    if include_live && inputs.presentation.tab == Tab::Conversation {
        let requests = live_tail_responses(inputs, inputs.presentation.agent);
        live_tail(inputs, requests, &mut entries);
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::super::tests::{replay, update};
    use super::*;
    use crate::tests::bounded;
    use crate::tui::app::{
        App,
        played::{
            answered, ask, at, call, committed, completed, count, exchange, failing_call,
            latest_request, model_failed, observe_until, played_fixture, recorded, requested,
            results, stopped, streamed,
        },
        tests::{journal, whole_entries},
    };
    use serde_json::json;
    use skyhook::agent::RuntimeEvent;
    use skyhook::provider::ProviderErrorKind;
    use skyhook::provider::protocol::{AssistantItem, Completion, CutReason, ItemKind};
    use skyhook::session::AttemptFailure;

    /// An answer's footer times its response, and the turn when a tool round began
    /// it; an answer that is a turn of its own shows only its response.
    #[tokio::test]
    async fn answer_footers_time_their_response_and_a_longer_turn() {
        let (_root, mut app, mut model, session) = played_fixture().await;
        let first = ask(&session, "first");
        let exec = call("call", "exec", json!({"command": ["true"]}), 0);
        model.turn().await.end(vec![exec]);
        model.turn().await.answer("done");
        bounded(first).await.unwrap().unwrap();
        let answer = vec![AssistantItem::text("text", 0, "done")];
        exchange(&mut app, &mut model, "second", answer).await;
        observe_until(&mut app, |app| count(&app.journal, answered) == 2).await;
        let records = journal(&session).await;
        let seconds = super::super::span;
        let response = seconds(at(&records, requested, 1), at(&records, completed, 1));
        let turn = seconds(at(&records, requested, 0), at(&records, completed, 1));
        let alone = seconds(at(&records, requested, 2), at(&records, completed, 2));
        let footers = |entries: &[Entry]| -> Vec<_> {
            let footers = entries.iter().filter_map(|entry| entry.footer.as_deref());
            footers.map(str::to_owned).collect()
        };
        // The tool round between the turn's requests ran a process.
        assert!(at(&records, requested, 0) < at(&records, requested, 1));
        let expected = [
            format!("fixture · {response} · turn {turn}"),
            format!("fixture · {alone}"),
        ];
        assert_eq!(footers(&whole_entries(&mut app, false).await), expected);
        // As the updates showed them.
        app.rebuild_content();
        assert_eq!(footers(app.entries()), expected);
    }

    /// A tool card follows its job by the job's exact call, so a call ID reused
    /// by a later response that fails before any job starts gets a card of its
    /// own.
    #[tokio::test]
    async fn admitted_results_use_exact_job_provenance() {
        let (_root, mut app, mut model, _session) = played_fixture().await;
        let asked = ask(app.session().unwrap(), "run");
        let admitted = call("reused", "exec", json!({"command": ["true"]}), 0);
        model.turn().await.end(vec![admitted]);
        model.turn().await.end(vec![failing_call("reused")]);
        model.turn().await.answer("done");
        bounded(asked).await.unwrap().unwrap();
        observe_until(&mut app, |app| count(&app.journal, committed(results)) == 2).await;
        let job = (app.journal.iter())
            .find_map(|record| match &record.event {
                SessionEvent::JobCreated { job, .. } => Some(*job),
                _ => None,
            })
            .unwrap();
        let cards: Vec<_> = whole_entries(&mut app, false)
            .await
            .into_iter()
            .filter(|card| card.surface == Surface::Tool)
            .collect();
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].job_id(), Some(job));
        assert_eq!(
            (cards[1].job_id(), cards[1].text()),
            (None, "▸ × exec · Failed")
        );
        assert_ne!(cards[0].key(), cards[1].key());
        assert!(cards[1].expandable());
    }

    #[tokio::test]
    async fn whitespace_only_turns_neither_create_agent_cards_nor_steal_the_answer_footer() {
        let (_root, mut app, mut model, session) = played_fixture().await;
        let _asked = ask(&session, "look");
        let reasoning = "Inspect the repository.";
        let whitespace = ["\n\n", "\n\n\n", " \t\r\n", "\u{2003}"];
        for (index, whitespace) in whitespace.into_iter().enumerate() {
            let read = call(&format!("read-{index}"), "read", json!({"path": "."}), 2);
            model.turn().await.end(vec![
                AssistantItem::reasoning("reasoning", 0, reasoning, Some(replay())),
                AssistantItem::text("separator", 1, whitespace),
                read,
            ]);
        }
        let answer = "  Actual answer with spacing.\n";
        model.turn().await.end(vec![
            AssistantItem::text("answer", 0, answer),
            AssistantItem::text("separator", 1, "\n\n"),
        ]);
        observe_until(&mut app, |app| recorded(&app.journal, answered)).await;

        let cards = whole_entries(&mut app, true).await;
        let surfaced = |surface, text| {
            let cards = cards.iter();
            cards
                .filter(|card| card.surface == surface && card.text().contains(text))
                .count()
        };
        assert_eq!(surfaced(Surface::Reasoning, reasoning), whitespace.len());
        assert_eq!(surfaced(Surface::Tool, "read"), whitespace.len());
        let cards = whole_entries(&mut app, false).await;
        let answers: Vec<_> = cards
            .iter()
            .filter(|card| card.surface == Surface::Agent)
            .collect();
        assert_eq!(answers.len(), 1);
        assert!(answers[0].text().ends_with(answer));
        let footer = answers[0].footer.as_deref().unwrap();
        assert!(footer.starts_with("fixture · "), "{footer}");
    }

    #[tokio::test]
    async fn refusals_reach_the_transcript_as_errors_through_the_real_projection() {
        // Guards the journal-to-card wiring: rendering a refusal as an ordinary
        // failure would restore the silent-failure UX this classification exists
        // to prevent.
        let (_root, mut app, mut model, session) = played_fixture().await;
        let asked = ask(&session, "question");
        let refusal = Completion::cut(Vec::new(), CutReason::Refusal).unwrap();
        model.turn().await.complete(refusal);
        let _ = bounded(asked).await.unwrap();
        observe_until(&mut app, |app| count(&app.journal, model_failed) == 1).await;
        let refused_request = latest_request(&app.journal).unwrap();
        let detail = (app.journal.iter()).find_map(|record| match &record.event {
            SessionEvent::ModelFailed {
                failure: AttemptFailure::Final(skyhook::agent::Failure::Refused(detail)),
                ..
            } => Some(detail.clone()),
            _ => None,
        });
        let detail = detail.expect("a refusal is journaled as one");
        let entries = whole_entries(&mut app, false).await;
        let card = entries
            .iter()
            .find(|entry| entry.key() == &EntryKey::Retry(refused_request))
            .expect("a refusal is presented in the conversation");
        assert_eq!(card.surface, Surface::Error);
        let mut lines = card.text().lines();
        assert_eq!(lines.next(), Some("Model declined to respond · attempt 1"));
        assert_eq!(lines.next(), Some(detail.as_str()));
        assert_eq!(lines.next(), Some(super::super::retry::REFUSAL_HINT));
        assert_eq!(lines.next(), None);
        // An ordinary failure keeps the muted presentation and gains no hint.
        let (_root, mut app, mut model, session) = played_fixture().await;
        let asked = ask(&session, "question");
        let rejected = ProviderErrorKind::Protocol.error("rejected");
        model.turn().await.fail(rejected);
        let _ = bounded(asked).await.unwrap();
        observe_until(&mut app, |app| count(&app.journal, model_failed) == 1).await;
        let failed_request = latest_request(&app.journal).unwrap();
        let entries = whole_entries(&mut app, false).await;
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

    /// A partial attempt shows where it settled until its agent's next turn begins,
    /// which lets go of it as a reopen does; the current stream stays last.
    #[tokio::test]
    async fn partial_attempts_show_until_the_next_turn_while_the_current_stream_stays_last() {
        let (_root, mut app, mut model, session) = played_fixture().await;
        let agent = app.root_agent().clone();
        let shown = async |app: &mut App| {
            let entries = whole_entries(app, false).await;
            let text = entries.iter().map(Entry::text).collect::<Vec<_>>();
            (text.join("\n"), entries)
        };
        let asked = ask(&session, "first");
        let turn = model.turn().await;
        turn.delta("text", ItemKind::Text, "failed partial");
        observe_until(&mut app, |app| streamed(&app.snapshot, "failed partial")).await;
        turn.fail(ProviderErrorKind::Protocol.error("stream lost"));
        let _ = bounded(asked).await.unwrap();
        observe_until(&mut app, |app| count(&app.journal, model_failed) == 1).await;
        assert_eq!(shown(&mut app).await.0.matches("failed partial").count(), 1);
        let continued = tokio::spawn({
            let session = session.clone();
            async move { session.continue_turn().await }
        });
        model.turn().await.answer("successful retry");
        bounded(continued).await.unwrap().unwrap();
        let asked = ask(&session, "new followup");
        let turn = model.turn().await;
        turn.delta("reasoning", ItemKind::Reasoning, "interrupted thinking");
        turn.delta("text", ItemKind::Text, "interrupted partial");
        observe_until(&mut app, |app| {
            streamed(&app.snapshot, "interrupted partial")
        })
        .await;
        session.interrupt().await;
        let _ = bounded(asked).await.unwrap();
        observe_until(&mut app, |app| stopped(&app.snapshot, &agent)).await;
        let (text, _) = shown(&mut app).await;
        let order = [
            "successful retry",
            "new followup",
            "interrupted thinking",
            "Incomplete response\ninterrupted partial",
        ];
        for pair in order.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            assert!(
                text.find(a).unwrap() < text.find(b).unwrap(),
                "{a} must precede {b}: {text}"
            );
        }
        assert!(!text.contains("failed partial"), "{text}");
        let _continued = tokio::spawn(async move { session.continue_turn().await });
        let turn = model.turn().await;
        turn.delta("text", ItemKind::Text, "current stream");
        observe_until(&mut app, |app| streamed(&app.snapshot, "current stream")).await;
        // A child's stream on the same request never shows in its parent's conversation.
        let child = RuntimeEvent::ResponseEvent {
            agent: agent.child(1),
            request: latest_request(&app.journal).unwrap(),
            event: skyhook::provider::protocol::ResponseEvent::Delta {
                block: skyhook::provider::protocol::BlockRef::single("text"),
                kind: ItemKind::Text,
                text: "child only".into(),
            },
        };
        update(&mut app.snapshot, child);
        let (text, entries) = shown(&mut app).await;
        assert!(!text.contains("interrupted partial"), "{text}");
        assert!(!text.contains("child only"));
        let last = entries
            .iter()
            .rev()
            .find(|entry| !matches!(entry.key(), EntryKey::Working(_)));
        assert!(last.unwrap().text().contains("current stream"));
    }

    #[tokio::test]
    async fn retry_card_is_one_compact_safe_entry_and_leaves_the_stored_error_intact() {
        let (_root, mut app, mut model, session) = played_fixture().await;
        let _asked = ask(&session, "question");
        let error = format!("HTTP 503 overloaded\n\u{1b}[31m{}", "x".repeat(1000));
        let overloaded = ProviderErrorKind::Unavailable.error(error.clone());
        model.turn().await.fail(overloaded);
        let scheduled = |event: &SessionEvent| {
            matches!(
                event,
                SessionEvent::ModelFailed {
                    failure: AttemptFailure::Retrying { .. },
                    ..
                }
            )
        };
        observe_until(&mut app, |app| recorded(&app.journal, scheduled)).await;
        let request = latest_request(&app.journal).unwrap();
        let cards = whole_entries(&mut app, false).await;
        let mut retries = cards
            .iter()
            .filter(|card| card.key() == &EntryKey::Retry(request));
        let text = retries.next().unwrap().text();
        assert!(retries.next().is_none());
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("attempt 2") && text.contains("HTTP 503 overloaded"));
        assert!(!text.contains('\u{1b}') && text.chars().count() < 320 && text.ends_with('…'));
        let stored = |event: &SessionEvent| {
            matches!(event, SessionEvent::ModelFailed {
                failure: AttemptFailure::Retrying { message, .. }, ..
            } if *message == error)
        };
        assert!(recorded(&app.journal, stored));
        // Interrupting the backoff ends the turn, and with it the retry's countdown.
        session.interrupt().await;
        let ended = |event: &SessionEvent| matches!(event, SessionEvent::AgentInterrupted);
        observe_until(&mut app, |app| recorded(&app.journal, ended)).await;
        let cards = whole_entries(&mut app, false).await;
        let card = cards
            .iter()
            .find(|card| card.key() == &EntryKey::Retry(request));
        assert_eq!(card.unwrap().text(), "Interrupted · attempt 1");
    }
}

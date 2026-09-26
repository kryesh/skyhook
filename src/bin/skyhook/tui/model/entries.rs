//! Tab selection and ordered conversation projection, including call/result provenance.
//!
//! History is a fold over sources: journal records, or jobs on the Jobs tab. Each
//! source owns a contiguous segment of entries, rebuilt only when something it was
//! built from changes, so a new record costs its own entries, not the history's.
use crate::tui::app::OutputStore;

use super::jobs::{call_entry, job_entry};
use super::live::{
    block_key, live_tail_responses, reasoning_entry, response_entries, working_entry,
};
use super::notifications::job_event_entries;
use super::projection::JobInfo;
use super::requests::request_entry;
use super::{
    Entry, EntryKey, EntryView, Projection, ResponseRef, Surface, Tab, Title, number, pretty,
};
use skyhook::agent::ObservationSnapshot;
use skyhook::identity::JobId;
use skyhook::provider::protocol::{AssistantItem, BlockRef, ToolResult};
use skyhook::session::{
    EventRecord, JobEvent, Message, MessageSeq, RecordSeq, RequestPhase, RequestSeq, SessionEvent,
    UserPart,
};
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
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Source {
    Record(RecordSeq),
    Job(JobId),
}

/// Where a source's segment sorts: its parent job's place, then its own source, so
/// a script's children sit directly under it even while scripts run in parallel.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
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
}

struct Segment {
    place: Place,
    start: usize,
    len: usize,
}

/// A job card's place and tool group, kept so its children can inherit them.
struct Placed {
    place: Place,
    group: ToolGroup,
}

/// One source's entries, their tool groups, and what they were built from.
#[derive(Default)]
struct Built {
    entries: Vec<Entry>,
    groups: Vec<Option<ToolGroup>>,
    deps: Vec<Dep>,
}
impl Built {
    fn push(&mut self, entry: Entry, group: Option<ToolGroup>) {
        self.entries.push(entry);
        self.groups.push(group);
    }
}

/// Everything history is built from.
#[derive(Clone, Copy)]
pub struct Inputs<'a> {
    pub snapshot: &'a ObservationSnapshot,
    pub projection: &'a Projection,
    pub presentation: EntryView<'a>,
    pub outputs: &'a OutputStore,
}

/// Tool results carry a call ID but no message sequence. Each pairs with a call of
/// the agent's most recent assistant turn, consuming the call once, so an old or
/// reused ID never claims a later result.
#[derive(Default)]
struct Pairing {
    pending: HashMap<(String, String), MessageSeq>,
    turn: Option<MessageSeq>,
    /// A call's result, as its record and index.
    results: HashMap<(MessageSeq, String), (RecordSeq, usize)>,
    matched: HashSet<(RecordSeq, usize)>,
}
impl Pairing {
    /// Fold one record, noting the calls it paired with a result.
    fn observe(&mut self, record: &EventRecord, paired: &mut HashSet<Dep>) {
        match &record.event {
            SessionEvent::MessageCommitted {
                message: Message::Assistant(items),
            } => {
                self.pending.clear();
                self.turn = Some(record.sequence.message());
                for call in items.iter().filter_map(|item| item.call()) {
                    self.pending.insert(
                        (call.id().to_owned(), call.name().to_owned()),
                        record.sequence.message(),
                    );
                }
            }
            SessionEvent::JobCreated {
                origin: Some(origin),
                tool,
                ..
            } if self.turn.is_none_or(|turn| turn <= origin.message) => {
                // Retained job provenance also identifies a call whose assistant
                // message is no longer in the retained history.
                let message = origin.message;
                self.pending
                    .insert((origin.call_id.clone(), tool.clone()), message);
                self.turn = Some(message);
            }
            SessionEvent::MessageCommitted {
                message: Message::Tool(results),
            } => {
                for (index, result) in results.iter().enumerate() {
                    let call = (result.call_id.clone(), result.name.clone());
                    if let Some(message) = self.pending.remove(&call) {
                        let call = result.call_id.clone();
                        self.results
                            .insert((message, call.clone()), (record.sequence, index));
                        self.matched.insert((record.sequence, index));
                        paired.insert(Dep::Call(message, call));
                    }
                }
            }
            _ => {}
        }
    }

    fn result<'a>(
        &self,
        snapshot: &'a ObservationSnapshot,
        message: MessageSeq,
        call: &str,
    ) -> Option<&'a ToolResult> {
        let (record, index) = *self.results.get(&(message, call.to_owned()))?;
        match &snapshot.records.get(&record)?.event {
            SessionEvent::MessageCommitted {
                message: Message::Tool(results),
            } => results.get(index),
            _ => None,
        }
    }
}

/// What an update changed: entries replaced in place, and the first entry from
/// which later entries moved.
#[derive(Default)]
pub struct Changed {
    pub dirty: Vec<usize>,
    pub shifted: Option<usize>,
}
impl Changed {
    fn shift(&mut self, from: usize) {
        self.shifted = Some(self.shifted.map_or(from, |shifted| shifted.min(from)));
    }
}

/// Retained history for one agent and tab.
#[derive(Default)]
pub struct History {
    /// How many of the agent's records have been folded.
    folded: usize,
    pairing: Pairing,
    segments: Vec<Segment>,
    index: HashMap<Source, usize>,
    deps: HashMap<Dep, Vec<Source>>,
    /// Each entry's tool group, for the spacing between siblings.
    groups: Vec<Option<ToolGroup>>,
    /// Each job card's place and group.
    jobs: HashMap<JobId, Placed>,
}

impl History {
    pub fn len(&self) -> usize {
        self.groups.len()
    }

    /// A fresh history and its entries.
    pub fn build(inputs: Inputs<'_>) -> (Self, Vec<Entry>) {
        let mut history = Self::default();
        let mut entries = Vec::new();
        let candidates = inputs.projection.jobs.keys().copied().collect();
        // Every record is folded before any segment is built: a call's segment
        // shows the result that arrives after it.
        let places = history.fold(inputs, candidates, &mut HashSet::new());
        for place in places {
            history.insert(place, inputs, &mut entries, &mut Changed::default());
        }
        (history, entries)
    }

    /// Fold the agent's new records and rebuild what `changed` names. `entries` holds
    /// exactly this history's entries.
    pub fn update(
        &mut self,
        inputs: Inputs<'_>,
        entries: &mut Vec<Entry>,
        mut changed: HashSet<Dep>,
    ) -> Changed {
        let candidates = changed
            .iter()
            .filter_map(|dep| match dep {
                Dep::Job(job) => Some(*job),
                _ => None,
            })
            .collect();
        let places = self.fold(inputs, candidates, &mut changed);
        let mut result = Changed::default();
        let mut positions: Vec<_> = changed
            .iter()
            .filter_map(|dep| self.deps.get(dep))
            .flatten()
            .filter_map(|source| self.index.get(source).copied())
            .collect();
        positions.sort_unstable();
        positions.dedup();
        for position in positions {
            self.rebuild(position, inputs, entries, &mut result);
        }
        for place in places {
            self.insert(place, inputs, entries, &mut result);
        }
        result
    }

    /// The entries of a source, if it has any.
    pub fn record_entries(&self, record: RecordSeq) -> Option<Range<usize>> {
        let segment = &self.segments[*self.index.get(&Source::Record(record))?];
        Some(segment.start..segment.start + segment.len)
    }

    /// Places of new sources in order: the agent's unfolded records, or its jobs
    /// among `candidates` on the Jobs tab.
    fn fold(
        &mut self,
        inputs: Inputs<'_>,
        candidates: Vec<JobId>,
        changed: &mut HashSet<Dep>,
    ) -> Vec<Place> {
        let Inputs {
            snapshot,
            projection,
            presentation,
            ..
        } = inputs;
        let agent = presentation.agent;
        if presentation.tab == Tab::Jobs {
            let mut jobs: Vec<_> = candidates
                .into_iter()
                .filter(|job| !self.index.contains_key(&Source::Job(*job)))
                .filter_map(|job| projection.jobs.get(&job))
                .filter(|job| &job.agent == agent)
                .collect();
            // IDs follow creation, so a script is placed before its children.
            jobs.sort_unstable_by_key(|job| job.id);
            return jobs
                .into_iter()
                .map(|job| self.place_job(Source::Job(job.id), job, None))
                .collect();
        }
        let records = projection.records_by_agent.get(agent);
        let records = records.map_or(&[][..], Vec::as_slice);
        let new = &records[self.folded.min(records.len())..];
        self.folded = records.len();
        let mut places = Vec::new();
        for record in new
            .iter()
            .filter_map(|sequence| snapshot.records.get(sequence))
        {
            let source = Source::Record(record.sequence);
            match presentation.tab {
                Tab::Conversation => self.pairing.observe(record, changed),
                Tab::Requests if !matches!(record.event, SessionEvent::ModelRequested { .. }) => {
                    continue;
                }
                _ => {}
            }
            let job = match &record.event {
                SessionEvent::JobCreated { job, origin, .. }
                    if presentation.tab == Tab::Conversation =>
                {
                    projection.jobs.get(job).map(|job| (job, origin))
                }
                _ => None,
            };
            places.push(match job {
                Some((job, origin)) => {
                    let origin = origin.as_ref().map(|origin| origin.message);
                    self.place_job(source, job, origin)
                }
                None => Place::under(None, source),
            });
        }
        places
    }

    /// A child job follows its parent and shares its tool group, so children sit
    /// tight under their script and the script's block against its siblings.
    /// Other jobs group with the response that called them, if any.
    fn place_job(&mut self, source: Source, job: &JobInfo, origin: Option<MessageSeq>) -> Place {
        let parent = job.parent.and_then(|parent| self.jobs.get(&parent));
        let group = parent.map_or_else(
            || origin.map_or(ToolGroup::Job(job.id), ToolGroup::Response),
            |parent| parent.group,
        );
        let place = Place::under(parent.map(|parent| &parent.place), source);
        let placed = Placed {
            place: place.clone(),
            group,
        };
        self.jobs.insert(job.id, placed);
        place
    }

    /// Place a new source's segment in place order.
    fn insert(
        &mut self,
        place: Place,
        inputs: Inputs<'_>,
        entries: &mut Vec<Entry>,
        result: &mut Changed,
    ) {
        let source = place.source();
        let position = self
            .segments
            .partition_point(|segment| segment.place < place);
        let start = self
            .segments
            .get(position)
            .map_or(entries.len(), |segment| segment.start);
        let built = self.build_source(source, inputs);
        self.register(source, &built.deps);
        let len = built.entries.len();
        entries.splice(start..start, built.entries);
        self.groups.splice(start..start, built.groups);
        self.segments
            .insert(position, Segment { place, start, len });
        for later in &mut self.segments[position + 1..] {
            later.start += len;
        }
        for (offset, segment) in self.segments[position..].iter().enumerate() {
            self.index.insert(segment.place.source(), position + offset);
        }
        if position + 1 < self.segments.len() && len > 0 {
            result.shift(start);
        }
        self.compact(
            inputs,
            entries,
            start.saturating_sub(1)..start + len,
            result,
        );
    }

    fn rebuild(
        &mut self,
        position: usize,
        inputs: Inputs<'_>,
        entries: &mut Vec<Entry>,
        result: &mut Changed,
    ) {
        let segment = &self.segments[position];
        let (source, start, len) = (segment.place.source(), segment.start, segment.len);
        let built = self.build_source(source, inputs);
        self.register(source, &built.deps);
        let new_len = built.entries.len();
        let old: Vec<_> = entries.splice(start..start + len, built.entries).collect();
        self.groups.splice(start..start + len, built.groups);
        self.compact(
            inputs,
            entries,
            start.saturating_sub(1)..start + new_len,
            result,
        );
        if new_len == len {
            let changed = old.iter().zip(&entries[start..]).enumerate();
            let changed = changed.filter(|(_, (old, new))| old != new);
            result
                .dirty
                .extend(changed.map(|(offset, _)| start + offset));
        } else {
            self.segments[position].len = new_len;
            for later in &mut self.segments[position + 1..] {
                later.start = later.start - len + new_len;
            }
            result.shift(start);
        }
    }

    fn register(&mut self, source: Source, deps: &[Dep]) {
        for dep in deps {
            let sources = self.deps.entry(dep.clone()).or_default();
            if !sources.contains(&source) {
                sources.push(source);
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
        result: &mut Changed,
    ) {
        if inputs.presentation.tab != Tab::Conversation {
            return;
        }
        for index in range.start..range.end.min(entries.len()) {
            let next = self.groups.get(index + 1).copied().flatten();
            let compact = self.groups[index].is_some_and(|group| next == Some(group));
            if entries[index].compact_after != compact {
                entries[index].compact_after = compact;
                result.dirty.push(index);
            }
        }
    }

    /// A job's card, indented under its parents, and its tool group.
    fn job_card(
        &self,
        job: JobId,
        inputs: Inputs<'_>,
        built: &mut Built,
    ) -> Option<(Entry, ToolGroup)> {
        built.deps.push(Dep::Job(job));
        let info = inputs.projection.jobs.get(&job)?;
        let placed = self.jobs.get(&job)?;
        let EntryView {
            view, all_details, ..
        } = inputs.presentation;
        let mut entry = job_entry(info, inputs.projection, view, inputs.outputs, all_details);
        entry.indent = (placed.place.depth().min(8) * 2) as u16;
        Some((entry, placed.group))
    }

    fn build_source(&self, source: Source, inputs: Inputs<'_>) -> Built {
        let Inputs {
            snapshot,
            projection,
            presentation,
            ..
        } = inputs;
        let mut built = Built::default();
        match source {
            Source::Job(job) => {
                if let Some((mut entry, _)) = self.job_card(job, inputs, &mut built) {
                    // The Jobs tab is a dense list; conversation grouping owns its
                    // spacing separately, and expanded documents stay unchanged.
                    entry.compact_after = true;
                    built.push(entry, None);
                }
            }
            Source::Record(sequence) => {
                if let Some(record) = snapshot.records.get(&sequence) {
                    if presentation.tab == Tab::Requests {
                        let request = sequence.request();
                        built.deps.push(Dep::Request(request));
                        if let Some(record) = projection.ledger.get(request) {
                            built.push(request_entry(request, record), None);
                        }
                    } else {
                        self.conversation(record, inputs, &mut built);
                    }
                }
            }
        }
        built
    }

    fn conversation(&self, record: &EventRecord, inputs: Inputs<'_>, built: &mut Built) {
        let Inputs {
            snapshot,
            projection,
            presentation,
            ..
        } = inputs;
        let EntryView {
            agent,
            view,
            all_details,
            ..
        } = presentation;
        let agent_name = projection.agent_name(agent);
        let key = EntryKey::Record(record.sequence);
        match &record.event {
            SessionEvent::ModelRequested { .. } => {
                let request = record.sequence.request();
                built.deps.push(Dep::Request(request));
                if let Some(entry) = super::retry::retry_entry(snapshot, projection, agent, request)
                {
                    built.push(entry, None);
                }
                // A settled response no commit replaced (an interrupted attempt)
                // stays at its journal position; a failure's is part of its card.
                let phase = projection.ledger.get(request).map(|record| &record.phase);
                if matches!(
                    phase,
                    Some(RequestPhase::Interrupted { .. } | RequestPhase::Completed { .. })
                ) && let Some(response) = snapshot.responses.get(&(agent.clone(), request))
                    && response.settlement().is_some()
                {
                    for entry in response_entries(request, response, view, agent_name) {
                        built.push(entry, None);
                    }
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
                                built.deps.extend(events.iter().filter_map(|event| {
                                    match event {
                                        JobEvent::Message(message) => Some(message.id),
                                        JobEvent::Job(job) => job.id(),
                                    }
                                    .map(Dep::Job)
                                }));
                                let group = ToolGroup::Notification(record.sequence, i);
                                let entries = job_event_entries(
                                    record.sequence,
                                    i,
                                    events,
                                    projection,
                                    view,
                                    all_details,
                                );
                                for entry in entries {
                                    built.push(entry, Some(group));
                                }
                                continue;
                            }
                            // Persisted runtime state is model context, not conversation.
                            UserPart::State { .. } => continue,
                            UserPart::Compaction { text } => {
                                ("Compaction", text.clone(), Surface::Muted)
                            }
                        };
                        let key = EntryKey::UserBlock {
                            record: record.sequence,
                            index: i,
                        };
                        let entry = Entry::titled(key, Title::plain(title), text, surface);
                        built.push(entry, None);
                    }
                }
                Message::Assistant(items) => {
                    self.assistant(record, items, inputs, built);
                }
                Message::Tool(results) => {
                    for (index, result) in results.iter().enumerate() {
                        if !self.pairing.matched.contains(&(record.sequence, index)) {
                            let result_key = EntryKey::ToolResult {
                                record: record.sequence,
                                call: result.call_id.clone(),
                            };
                            let open = view.is_expanded(&result_key, all_details);
                            let call = (result.name.as_str(), None, Some(result));
                            let entry = call_entry(result_key, call, agent, projection, open);
                            built.push(entry, None);
                        }
                    }
                }
            },
            SessionEvent::JobCreated { job, .. } => {
                if let Some((entry, group)) = self.job_card(*job, inputs, built) {
                    built.push(entry, Some(group));
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
                built.push(Entry::titled(key, title, body, Surface::Muted), None);
            }
            SessionEvent::Status { message } => {
                let entry = Entry::new(key, format!("Status · {message}"), Surface::Status);
                built.push(entry, None);
            }
            SessionEvent::CompactionFailed { error, .. } => {
                let text = format!("Compaction failed; previous context retained\n{error}");
                built.push(Entry::new(key, text, Surface::Error), None);
            }
            SessionEvent::CompactionSkipped { reason, .. } => {
                let text = format!("Compaction skipped · {reason}");
                built.push(Entry::new(key, text, Surface::Muted), None);
            }
            _ => {}
        }
    }

    fn assistant(
        &self,
        record: &EventRecord,
        items: &[AssistantItem],
        inputs: Inputs<'_>,
        built: &mut Built,
    ) {
        let Inputs {
            snapshot,
            projection,
            presentation,
            ..
        } = inputs;
        let EntryView {
            agent,
            view,
            all_details,
            ..
        } = presentation;
        let agent_name = projection.agent_name(agent);
        let message = record.sequence.message();
        let request = projection.ledger.request_of(message);
        built.deps.extend(request.map(Dep::Request));
        let response = request.map_or(ResponseRef::Message(message), ResponseRef::Request);
        let footer = request
            .and_then(|request| projection.ledger.get(request))
            .map(|request| request.profile.profile.model.clone());
        // The model footer sits under the last visible text of an answer; a working
        // turn (one with calls) has none.
        let final_text = (!items.iter().any(|item| item.call().is_some()))
            .then(|| {
                items.iter().rev().find_map(|item| match item {
                    AssistantItem::Text { blocks, .. } => {
                        blocks.iter().rfind(|block| !block.text.trim().is_empty())
                    }
                    _ => None,
                })
            })
            .flatten();
        for item in items {
            match item {
                AssistantItem::Text { id, blocks, .. } => {
                    for block in blocks.iter().filter(|block| !block.text.trim().is_empty()) {
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
                        if final_text.is_some_and(|last| std::ptr::eq(last, block)) {
                            entry.footer.clone_from(&footer);
                        }
                        built.push(entry, None);
                    }
                }
                AssistantItem::Reasoning { id, blocks, .. } => {
                    for block in blocks.iter().filter(|block| !block.text.trim().is_empty()) {
                        let block_ref = BlockRef {
                            item: id.clone(),
                            block: block.id.clone(),
                        };
                        let entry = reasoning_entry(
                            block_key(response, &block_ref),
                            &block.text,
                            view,
                            super::live::ReasoningStatus::Complete,
                        );
                        built.push(entry, None);
                    }
                }
                AssistantItem::ToolCall { call, .. } => {
                    let id = call.id().to_owned();
                    built.deps.push(Dep::Call(message, id.clone()));
                    // An admitted call is shown by its job's card instead.
                    let admitted = (agent.clone(), message, id);
                    if !projection.tool_origins.contains(&admitted) {
                        let key = EntryKey::ToolCall {
                            message,
                            call: admitted.2,
                        };
                        let open = view.is_expanded(&key, all_details);
                        let result = self.pairing.result(snapshot, message, call.id());
                        let call = (call.name(), Some(call.arguments()), result);
                        let entry = call_entry(key, call, agent, projection, open);
                        built.push(entry, Some(ToolGroup::Response(message)));
                    }
                }
            }
        }
    }
}

/// Shared by retained UI content and fresh export construction.
pub fn entries(
    snapshot: &ObservationSnapshot,
    projection: &Projection,
    presentation: EntryView<'_>,
    outputs: &OutputStore,
    include_live: bool,
) -> Vec<Entry> {
    let inputs = Inputs {
        snapshot,
        projection,
        presentation,
        outputs,
    };
    let (_, mut entries) = History::build(inputs);
    if include_live && presentation.tab == Tab::Conversation {
        let agent = presentation.agent;
        let agent_name = projection.agent_name(agent);
        let responses = live_tail_responses(snapshot, projection, agent);
        entries.extend(responses.into_iter().flat_map(|(request, response)| {
            response_entries(request, response, presentation.view, agent_name)
        }));
        let running = entries.iter().any(|entry| entry.running);
        entries.extend(working_entry(snapshot, projection, agent, running));
    }
    entries
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
                arguments: serde_json::json!({"command":["echo"]}),
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

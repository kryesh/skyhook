//! Presentation state and the stable interface to journal-derived content.

mod cache;
mod changes;
mod entries;
mod jobs;
mod live;
mod notifications;
mod projection;
mod requests;
mod retry;
mod transcript;
mod window;

pub use super::format::{Clean, Clock, Timing, clean, footer, number, pretty, span};
pub use cache::ContentCache;
pub use changes::{Changes, Splice};
pub use entries::entries;
pub use jobs::{state_glyph, state_name, state_role, target_suffix};
pub use projection::{
    AgentDisplayState, AgentInfo, Projection, WaitReason, continuable, open_jobs,
};
pub use requests::RequestRow;
pub use transcript::shown;
#[cfg(test)]
pub use window::KEPT;
pub use window::{
    CHILD_PAGE, ChildPage, Direction, Focus, LIST_PAGE, Loaded, Near, PAGE, Read, Want, Window,
};

use super::tool_view::{Document, Run};
pub use entries::Inputs;
use skyhook::identity::{AgentId, JobId};
use skyhook::provider::protocol::BlockRef;
use skyhook::session::{MessageSeq, RecordSeq, RequestSeq};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Tab {
    #[default]
    Conversation,
    Requests,
    Jobs,
}
impl Tab {
    pub fn next(self, backwards: bool) -> Self {
        let tabs = [Self::Conversation, Self::Requests, Self::Jobs];
        let n = tabs.iter().position(|t| *t == self).unwrap();
        tabs[(n + if backwards { tabs.len() - 1 } else { 1 }) % tabs.len()]
    }
}
/// The response a native block belongs to: its request, so the block keeps its
/// identity from live streaming through journal commit, or the committed message
/// when no request claims it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResponseRef {
    Request(RequestSeq),
    Message(MessageSeq),
}

/// Structural identity, independent of display prose and provider ID delimiters.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum EntryKey {
    Record(RecordSeq),
    UserBlock {
        record: RecordSeq,
        index: usize,
    },
    Block {
        response: ResponseRef,
        block: BlockRef,
    },
    /// A reasoning block's `section`th part, split at its standalone titles.
    Reasoning {
        response: ResponseRef,
        block: BlockRef,
        section: usize,
    },
    ToolCall {
        message: MessageSeq,
        call: String,
    },
    Notification {
        record: RecordSeq,
        block: usize,
        event: usize,
    },
    Request(RequestSeq),
    Retry(RequestSeq),
    Job(JobId),
    Working(AgentId),
    /// The status at this index of the app's unsaved statuses.
    UnsavedStatus(usize),
    /// The failure that stopped the session.
    JournalFailed,
    /// What the window has not loaded there.
    Gap(Gap),
}

/// Where a tab's history is not loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Gap {
    /// What lies before a list's first loaded entry.
    Earlier,
    /// What lies after a list's last loaded entry.
    Later,
    /// A parent card's children after those loaded.
    More(JobId),
}

/// A status shown but never journaled. It follows its agent's entry that was newest
/// when it arrived, by ordinal, so it scrolls away with the history around it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsavedStatus {
    pub agent: AgentId,
    pub after: Option<u64>,
    pub message: String,
}

/// A reading position that holds across layouts: on an entry by key, rows into it,
/// and on the tab's own entry it belongs to (a transcript entry, a request or a root
/// job), rows into what that shows, so it is found again when the entry is not. One
/// with no key is a place not loaded, which is read where it is.
#[derive(Clone, Debug)]
pub struct Anchor {
    pub key: Option<EntryKey>,
    pub offset: usize,
    pub place: Option<(Focus, usize)>,
}

impl Anchor {
    /// The top of the transcript entry `ordinal`.
    pub fn ordinal(ordinal: u64) -> Self {
        Self {
            key: None,
            offset: 0,
            place: Some((Focus::Ordinal(ordinal), 0)),
        }
    }

    /// Where a window loads to show the anchor; none for the newest entries.
    pub fn focus(&self) -> Option<Focus> {
        self.place.map(|(focus, _)| focus)
    }
}

#[derive(Default)]
pub struct View {
    /// Where the view's top sits; none while it follows the newest entries.
    pub top: Option<Anchor>,
    /// The selected entry.
    pub selected: Option<EntryKey>,
    /// Explicit expansion overrides; absent keys follow the caller's default.
    overrides: HashMap<EntryKey, bool>,
}
impl View {
    pub fn is_expanded(&self, key: &EntryKey, default: bool) -> bool {
        self.overrides.get(key).copied().unwrap_or(default)
    }

    pub fn set_expanded(&mut self, key: EntryKey, expanded: bool) {
        self.overrides.insert(key, expanded);
    }

    fn clear_collapsed(&mut self) {
        self.overrides.retain(|_, expanded| *expanded);
    }
}
/// Each agent's reading state, kept apart per inspector tab.
#[derive(Default)]
pub struct Views(HashMap<AgentId, HashMap<Tab, View>>);
impl Views {
    pub fn get(&self, agent: &AgentId, tab: Tab) -> Option<&View> {
        self.0.get(agent)?.get(&tab)
    }

    pub fn get_mut(&mut self, agent: &AgentId, tab: Tab) -> &mut View {
        let tabs = self.0.entry(agent.clone()).or_default();
        tabs.entry(tab).or_default()
    }

    /// Every tab's view of `from` becomes `to`'s.
    pub fn transfer(&mut self, from: &AgentId, to: AgentId) {
        if let Some(tabs) = self.0.remove(from) {
            self.0.insert(to, tabs);
        }
    }

    pub fn clear_collapsed(&mut self) {
        let views = self.0.values_mut().flat_map(HashMap::values_mut);
        views.for_each(View::clear_collapsed);
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    User,
    Agent,
    Reasoning,
    Tool,
    Muted,
    Status,
    Error,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disclosure {
    Open,
    Closed,
}
impl Disclosure {
    fn new(open: bool) -> Self {
        if open { Self::Open } else { Self::Closed }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Self::Open => "▾ ",
            Self::Closed => "▸ ",
        }
    }
}

/// A heading above an entry's body. A disclosure makes the entry
/// expandable and is drawn as its glyph; the spinner gutter is a render decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Title {
    pub label: Label,
    /// Plain text after the label, outside any Markdown styling.
    pub suffix: Option<&'static str>,
    pub disclosure: Option<Disclosure>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Label {
    Plain(Clean),
    /// Source rendered with the body's Markdown styling; `definitions` are link
    /// definitions its references resolve against without being part of it.
    Markdown {
        source: Clean,
        definitions: Clean,
    },
}

impl Title {
    fn new(label: Label, disclosure: Option<Disclosure>) -> Self {
        Self {
            label,
            suffix: None,
            disclosure,
        }
    }

    pub fn plain(label: impl Into<Clean>) -> Self {
        Self::new(Label::Plain(label.into()), None)
    }

    pub fn disclosed(label: impl Into<Clean>, open: bool) -> Self {
        Self::new(Label::Plain(label.into()), Some(Disclosure::new(open)))
    }

    pub fn markdown(source: impl Into<Clean>, definitions: impl Into<Clean>, open: bool) -> Self {
        let label = Label::Markdown {
            source: source.into(),
            definitions: definitions.into(),
        };
        Self::new(label, Some(Disclosure::new(open)))
    }

    /// What precedes the label; `gutter` reserves the spinner's two cells.
    pub fn prefix(&self, gutter: bool) -> String {
        let glyph = self.disclosure.map_or("", Disclosure::glyph);
        let gutter = if gutter { "  " } else { "" };
        format!("{glyph}{gutter}")
    }

    /// The label as source, and its suffix: the heading after its prefix.
    pub fn heading(&self) -> String {
        let (Label::Plain(label) | Label::Markdown { source: label, .. }) = &self.label;
        format!("{}{}", &**label, self.suffix.unwrap_or_default())
    }

    /// The heading as plain text, its label as source.
    pub fn line(&self) -> String {
        format!("{}{}", self.prefix(false), self.heading())
    }
}

/// One canonical payload owns all correlated presentation data.
#[derive(Clone, PartialEq, Eq)]
enum EntryBody {
    /// A titled or bare text body with its eager plain projection.
    Text {
        title: Option<Title>,
        body: Clean,
        plain: String,
    },
    Request {
        row: RequestRow,
        text: String,
    },
    Card(Card),
}

/// Header-free structured body and its eager, constructor-derived plain projection.
/// Neither the header nor the projection can be mutated independently.
#[derive(Clone, PartialEq, Eq)]
struct Card {
    header: Vec<Run>,
    body: Option<Document>,
    plain: String,
}
impl Card {
    /// The plain text leads with the disclosure glyph of whether a body
    /// exists; the header runs do not, as rendering draws it as decoration.
    fn new(header: Vec<Run>, body: Option<Document>) -> Self {
        let mut plain = Disclosure::new(body.is_some()).glyph().to_owned();
        for run in &header {
            plain.push_str(run.text());
        }
        if let Some(body) = &body {
            let body_text = body.plain_text();
            if !body.sections.is_empty() {
                plain.push('\n');
                plain.push_str(&body_text);
            }
        }
        Self {
            header,
            body,
            plain,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Entry {
    key: EntryKey,
    body: EntryBody,
    pub surface: Surface,
    pub timing: Timing,
    pub footer: Option<Clean>,
    pub indent: u16,
    /// Omit the separator before a related sibling tool or this script's first child.
    pub compact_after: bool,
}
impl Entry {
    pub fn key(&self) -> &EntryKey {
        &self.key
    }

    pub fn job_id(&self) -> Option<JobId> {
        match self.key {
            EntryKey::Job(id) => Some(id),
            _ => None,
        }
    }

    /// The whole entry as plain text, title included.
    pub fn text(&self) -> &str {
        match &self.body {
            EntryBody::Text { plain, .. } | EntryBody::Request { text: plain, .. } => plain,
            EntryBody::Card(card) => &card.plain,
        }
    }

    pub fn title(&self) -> Option<&Title> {
        match &self.body {
            EntryBody::Text { title, .. } => title.as_ref(),
            EntryBody::Request { .. } | EntryBody::Card(_) => None,
        }
    }

    /// The text under the title; the whole text of an untitled entry.
    pub fn body(&self) -> &str {
        match &self.body {
            EntryBody::Text { body, .. } => body,
            EntryBody::Request { text, .. } => text,
            EntryBody::Card(card) => &card.plain,
        }
    }

    pub fn request(&self) -> Option<&RequestRow> {
        match &self.body {
            EntryBody::Request { row, .. } => Some(row),
            _ => None,
        }
    }

    pub fn header(&self) -> Option<&[Run]> {
        match &self.body {
            EntryBody::Card(card) => Some(&card.header),
            _ => None,
        }
    }

    /// Expanded card body only: render the separate header exactly once.
    pub fn document(&self) -> Option<&Document> {
        match &self.body {
            EntryBody::Card(card) => card.body.as_ref(),
            _ => None,
        }
    }

    /// Expansion as projection built it: a title's disclosure, or whether a
    /// card carries its body.
    pub fn disclosure(&self) -> Option<Disclosure> {
        match &self.body {
            EntryBody::Text { title, .. } => title.as_ref()?.disclosure,
            EntryBody::Request { .. } => None,
            EntryBody::Card(card) => Some(Disclosure::new(card.body.is_some())),
        }
    }

    pub fn expandable(&self) -> bool {
        self.disclosure().is_some()
    }

    pub fn open(&self) -> bool {
        self.disclosure() == Some(Disclosure::Open)
    }

    /// Inline reasoning, the working indicator and what stands for entries not loaded
    /// are not navigable or copyable.
    pub fn selectable(&self) -> bool {
        self.expandable()
            || !(self.surface == Surface::Reasoning
                || matches!(self.key, EntryKey::Working(_) | EntryKey::Gap(_)))
    }

    /// Projection chooses key variants from semantic source kinds; untrusted
    /// text and historical values never choose the key/payload pairing.
    pub(crate) fn new(key: EntryKey, body: String, surface: Surface) -> Self {
        let body = Clean::from(body);
        Self {
            key,
            body: EntryBody::Text {
                title: None,
                plain: body.to_string(),
                body,
            },
            surface,
            timing: Timing::Untimed,
            footer: None,
            indent: 0,
            compact_after: false,
        }
    }

    pub(crate) fn titled(key: EntryKey, title: Title, body: String, surface: Surface) -> Self {
        let body = Clean::from(body);
        let mut plain = title.line();
        if !body.is_empty() {
            plain.push('\n');
            plain.push_str(&body);
        }
        Self {
            body: EntryBody::Text {
                title: Some(title),
                body,
                plain,
            },
            ..Self::new(key, String::new(), surface)
        }
    }

    pub(crate) fn card(key: EntryKey, header: Vec<Run>, body: Option<Document>) -> Self {
        Self {
            body: EntryBody::Card(Card::new(header, body)),
            ..Self::new(key, String::new(), Surface::Tool)
        }
    }

    pub(crate) fn request_entry(row: RequestRow) -> Self {
        let key = EntryKey::Request(row.sequence);
        Self {
            timing: row.timing,
            body: EntryBody::Request {
                text: row.metadata().join(" · "),
                row,
            },
            ..Self::new(key, String::new(), Surface::Tool)
        }
    }
}
#[derive(Clone, Copy)]
pub struct EntryView<'a> {
    pub agent: &'a AgentId,
    pub tab: Tab,
    pub view: &'a View,
    pub all_details: bool,
    /// The local day, on which times show without their date.
    pub today: chrono::NaiveDate,
}

#[cfg(test)]
mod tests {
    use super::super::tool_view::Run;
    use super::*;
    use crate::tui::{app::OutputStore, tool_view::tests::view};
    use skyhook::agent::{ObservedEvent, RuntimeEvent};
    use skyhook::identity::SessionId;
    use skyhook::provider::protocol::{Binding, Provenance, Replay, ReplayFormat, Scope};

    /// Install a job view of `fields` as `job`'s output through the store's refresh path.
    pub(super) fn loaded(outputs: &mut OutputStore, job: JobId, fields: serde_json::Value) {
        let (attempt, _) = outputs.begin(job).unwrap();
        outputs.complete(attempt, Ok(view(fields)));
    }

    pub(super) fn header_text(runs: &[Run]) -> String {
        runs.iter().map(Run::text).collect()
    }

    pub(super) fn root(seed: u8) -> AgentId {
        AgentId::root(SessionId::from_bytes([seed; 16]))
    }

    /// Apply one runtime event as the next revision.
    pub(super) fn update(snapshot: &mut skyhook::agent::ObservationSnapshot, event: RuntimeEvent) {
        snapshot.apply(ObservedEvent {
            revision: snapshot.revision + 1,
            event,
        });
    }

    pub(super) fn replay() -> Replay {
        Replay {
            provenance: Provenance {
                format: ReplayFormat::Messages,
                model: "fixture".into(),
                scope: Scope::try_from("reasoning".to_owned()).unwrap(),
            },
            payload: serde_json::json!({"signature": "opaque"}),
            binding: Binding::Free,
        }
    }
}

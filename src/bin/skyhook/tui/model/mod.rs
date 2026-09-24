//! Presentation state and the stable interface to journal-derived content.

mod cache;
mod entries;
mod jobs;
mod live;
mod notifications;
mod projection;
mod requests;
mod retry;

pub use super::format::{clean, footer, number, pretty};
pub use cache::{ContentCache, ContentChanges};
pub use entries::entries;
pub use jobs::{state_name, target_suffix};
pub use projection::{
    AgentDisplayState, AgentInfo, JobInfo, Projection, WaitReason, agent_footer, agent_footer_stats,
};
pub use requests::{RequestRow, RequestStatus};

use super::tool_view::{Document, Run};
use skyhook::identity::{AgentId, JobId};
use skyhook::provider::protocol::BlockRef;
use skyhook::session::{MessageSeq, RecordSeq, RequestSeq};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
    ToolCall {
        message: MessageSeq,
        call: String,
    },
    ToolResult {
        record: RecordSeq,
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
    UnsavedStatus(usize),
}

#[derive(Default)]
pub struct View {
    pub scroll: Option<usize>,
    pub row: usize,
    /// Explicit expansion overrides; absent keys follow the caller's default.
    overrides: HashMap<EntryKey, bool>,
    pub query: String,
}
impl View {
    pub fn is_expanded(&self, key: &EntryKey, default: bool) -> bool {
        self.overrides.get(key).copied().unwrap_or(default)
    }

    pub fn set_expanded(&mut self, key: EntryKey, expanded: bool) {
        self.overrides.insert(key, expanded);
    }

    pub fn clear_collapsed(&mut self) {
        self.overrides.retain(|_, expanded| *expanded);
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

/// A generated heading above an entry's body. A disclosure makes the entry
/// expandable and is drawn as its glyph; the spinner gutter is a render decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Title {
    pub label: String,
    pub disclosure: Option<Disclosure>,
}

impl Title {
    pub fn plain(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            disclosure: None,
        }
    }

    pub fn disclosed(label: impl Into<String>, open: bool) -> Self {
        Self {
            label: label.into(),
            disclosure: Some(if open {
                Disclosure::Open
            } else {
                Disclosure::Closed
            }),
        }
    }

    /// The heading line; `gutter` reserves the spinner's two cells before the label.
    pub fn line(&self, gutter: bool) -> String {
        let glyph = match self.disclosure {
            Some(Disclosure::Open) => "▾ ",
            Some(Disclosure::Closed) => "▸ ",
            None => "",
        };
        let gutter = if gutter { "  " } else { "" };
        format!("{glyph}{gutter}{}", self.label)
    }
}

/// One canonical payload owns all correlated presentation data.
#[derive(Clone, PartialEq, Eq)]
enum EntryBody {
    /// A titled or bare text body with its eager plain projection.
    Text {
        title: Option<Title>,
        body: String,
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
    fn new(header: Vec<Run>, body: Option<Document>) -> Self {
        let mut plain = String::new();
        for run in &header {
            super::format::push_clean(&mut plain, run.text());
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
    pub default_open: bool,
    pub running: bool,
    pub footer: Option<String>,
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

    pub fn expandable(&self) -> bool {
        match &self.body {
            EntryBody::Text { title, .. } => title
                .as_ref()
                .is_some_and(|title| title.disclosure.is_some()),
            EntryBody::Request { .. } => false,
            EntryBody::Card(_) => true,
        }
    }

    /// One expansion policy for card layout and interactive toggling.
    pub fn is_expanded(&self, view: &View, tab: Tab, details: bool) -> bool {
        let all = details
            && (self.job_id().is_some()
                || (tab == Tab::Conversation && self.surface == Surface::Tool));
        self.expandable() && view.is_expanded(self.key(), all || self.default_open)
    }

    /// Projection chooses key variants from semantic source kinds; untrusted
    /// text and historical values never choose the key/payload pairing.
    pub(crate) fn new(key: EntryKey, body: String, surface: Surface) -> Self {
        Self {
            key,
            body: EntryBody::Text {
                title: None,
                plain: body.clone(),
                body,
            },
            surface,
            default_open: false,
            running: false,
            footer: None,
            indent: 0,
            compact_after: false,
        }
    }

    pub(crate) fn titled(key: EntryKey, title: Title, body: String, surface: Surface) -> Self {
        let mut plain = title.line(false);
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
}

#[cfg(test)]
mod tests {
    use super::super::tool_view::Run;
    use super::*;
    use crate::tui::app::App;
    use skyhook::agent::{ObservationSnapshot, ObservedEvent, RuntimeEvent};
    use skyhook::identity::SessionId;
    use skyhook::job::{JobRole, JobState};
    use skyhook::provider::protocol::{
        AssistantItem, Binding, BlockId, BlockRef, ItemId, ItemKind, Provenance, Replay,
        ResponseEvent, Scope, ToolCall, ToolResult,
    };
    use skyhook::session::{
        Message, MessageSeq, ModelPurpose, RecordSeq, RequestSeq, SessionEvent, UserPart,
    };

    pub(super) fn header_text(runs: &[Run]) -> String {
        runs.iter().map(Run::text).collect()
    }

    pub(super) fn root(seed: u8) -> AgentId {
        AgentId::root(SessionId::from_bytes([seed; 16]))
    }

    /// Apply one runtime event as the next revision.
    pub(super) fn update(snapshot: &mut ObservationSnapshot, event: RuntimeEvent) {
        snapshot.apply(ObservedEvent {
            revision: snapshot.revision + 1,
            event,
        });
    }

    /// A journal whose sequences a real session minted: the app fixture's session
    /// appends every record, and the snapshot folds it as an observer would.
    pub(super) struct Journal {
        _root: tempfile::TempDir,
        app: App,
        pub(super) snapshot: ObservationSnapshot,
    }

    /// A journaled request and the model context it named.
    #[derive(Clone, Copy)]
    pub(super) struct Requested {
        pub(super) request: RequestSeq,
        pub(super) context: RecordSeq,
    }

    impl Journal {
        pub(super) async fn new() -> Self {
            let (root, app) = crate::tui::app::tests::fixture().await;
            Self {
                _root: root,
                app,
                snapshot: ObservationSnapshot::default(),
            }
        }

        pub(super) fn agent(&self) -> AgentId {
            self.app.session().unwrap().root_agent().clone()
        }

        pub(super) async fn record(&mut self, agent: &AgentId, event: SessionEvent) -> RecordSeq {
            let store = self.app.session().unwrap().store();
            let record = store.append(agent.clone(), event).await.unwrap();
            update(
                &mut self.snapshot,
                RuntimeEvent::Record(Box::new(record.clone())),
            );
            record.sequence
        }

        pub(super) fn response(
            &mut self,
            agent: &AgentId,
            request: RequestSeq,
            event: ResponseEvent,
        ) {
            let agent = agent.clone();
            update(
                &mut self.snapshot,
                RuntimeEvent::ResponseEvent {
                    agent,
                    request,
                    event,
                },
            );
        }

        /// Stream `text` into the `text` (or `reasoning`) item of a request.
        pub(super) fn delta(
            &mut self,
            agent: &AgentId,
            request: RequestSeq,
            item: &str,
            text: &str,
        ) {
            let kind = if item == "reasoning" {
                ItemKind::Reasoning
            } else {
                ItemKind::Text
            };
            let event = ResponseEvent::Delta {
                block: BlockRef {
                    item: ItemId::try_from(item.to_owned()).unwrap(),
                    block: BlockId::try_from(format!("{item}:0")).unwrap(),
                },
                kind,
                text: text.into(),
            };
            self.response(agent, request, event);
        }

        pub(super) async fn call_record(&mut self, agent: &AgentId, id: &str) -> MessageSeq {
            let args = serde_json::json!({"argv": ["echo", "  original\ttext\n"]});
            let call = ToolCall::new(id, "exec", args).unwrap();
            let message = Message::Assistant(vec![AssistantItem::tool_call("tool", 0, call)]);
            self.record(agent, SessionEvent::MessageCommitted { message })
                .await
                .message()
        }

        pub(super) async fn result_record(&mut self, agent: &AgentId, id: &str, error: bool) {
            let result = if error {
                serde_json::json!({"error": "Permission was denied", "code": "permission_denied", "executed": false})
            } else {
                serde_json::json!({"stdout": "  original\ttext\n"})
            };
            let message = Message::Tool(vec![ToolResult {
                call_id: id.into(),
                name: "exec".into(),
                result,
                images: vec![],
                is_error: error,
            }]);
            self.record(agent, SessionEvent::MessageCommitted { message })
                .await;
        }

        /// Record a model request and its first attempt, creating a fresh model
        /// context unless one is given.
        pub(super) async fn request(
            &mut self,
            agent: &AgentId,
            context: Option<RecordSeq>,
        ) -> Requested {
            let context = match context {
                Some(context) => context,
                None => {
                    let profile = skyhook::session::ProfileSnapshot {
                        name: "fixture".into(),
                        profile: skyhook::provider::profile::ModelProfile::new(
                            "fixture",
                            "fixture-model",
                            None,
                            128_000,
                            100,
                            false,
                        ),
                    };
                    let context = skyhook::session::ModelContext {
                        purpose: ModelPurpose::Agent,
                        profile,
                        system: vec![],
                        tools: vec![],
                        response_schema: None,
                    };
                    self.record(agent, SessionEvent::ModelContext { context })
                        .await
                }
            };
            let text = "original request".into();
            let message = Message::User(vec![UserPart::Text { text }]);
            let event = SessionEvent::ModelRequested {
                context,
                checkpoint: None,
                history: vec![],
                tail: vec![message],
                history_lifetime: Default::default(),
            };
            let request = self.record(agent, event).await.request();
            let attempt = skyhook::session::AttemptRef {
                request,
                attempt: 1,
            };
            self.record(agent, SessionEvent::ModelAttemptStarted(attempt))
                .await;
            Requested { request, context }
        }
    }

    pub(super) fn replay() -> Replay {
        Replay {
            provenance: Provenance {
                protocol: "fixture".into(),
                model: "fixture".into(),
                scope: Scope::try_from("reasoning".to_owned()).unwrap(),
            },
            payload: serde_json::json!({"signature": "opaque"}),
            binding: Binding::Free,
        }
    }

    pub(super) fn job_info(agent: &AgentId, id: u64, role: JobRole, state: JobState) -> JobInfo {
        JobInfo {
            id: JobId::new(id).unwrap(),
            agent: agent.clone(),
            name: None,
            tool: "exec".into(),
            role,
            activity: projection::JobActivity::Work,
            args: serde_json::json!({"argv": ["echo"]}),
            parent: None,
            state,
            location: skyhook::execution::ExecutionLocation::root("/workspace".into()),
            error: None,
        }
    }
}

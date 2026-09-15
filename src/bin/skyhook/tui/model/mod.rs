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
/// Structural identity, independent of display prose and provider ID delimiters.
/// Native blocks retain their request identity when committed; legacy records
/// without a matching request use their record sequence as the request fallback.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum EntryKey {
    Record(u64),
    UserBlock {
        record: u64,
        index: usize,
    },
    ResponseBlock {
        request: u64,
        item: String,
        block: String,
    },
    ReasoningBlock {
        request: u64,
        item: String,
        block: String,
    },
    ToolResult {
        record: u64,
        call: String,
    },
    Notification {
        record: u64,
        block: usize,
        event: Option<usize>,
    },
    Request(u64),
    Retry(u64),
    Job(JobId),
    Working(AgentId),
    UnsavedStatus(usize),
}

#[derive(Default)]
pub struct View {
    pub tab: Tab,
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
/// One canonical payload owns all correlated presentation data.
#[derive(Clone, PartialEq, Eq)]
enum EntryBody {
    Text { text: String, expandable: bool },
    Request { row: RequestRow, text: String },
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

    pub fn text(&self) -> &str {
        match &self.body {
            EntryBody::Text { text, .. } | EntryBody::Request { text, .. } => text,
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
            EntryBody::Text { expandable, .. } => *expandable,
            EntryBody::Request { .. } => false,
            EntryBody::Card(_) => true,
        }
    }

    /// One expansion policy for card layout and interactive toggling.
    pub fn is_expanded(&self, view: &View, details: bool) -> bool {
        let all = details
            && (self.job_id().is_some()
                || (view.tab == Tab::Conversation && self.surface == Surface::Tool));
        self.expandable() && view.is_expanded(self.key(), all || self.default_open)
    }

    /// Projection chooses key variants from semantic source kinds; untrusted
    /// text and historical values never choose the key/payload pairing.
    pub(crate) fn new(key: EntryKey, text: String, surface: Surface) -> Self {
        Self {
            key,
            body: EntryBody::Text {
                text,
                expandable: false,
            },
            surface,
            default_open: false,
            running: false,
            footer: None,
            indent: 0,
            compact_after: false,
        }
    }

    pub(crate) fn expandable_text(key: EntryKey, text: String, surface: Surface) -> Self {
        Self {
            body: EntryBody::Text {
                text,
                expandable: true,
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
    pub view: &'a View,
    pub thinking: bool,
    pub all_details: bool,
}

#[cfg(test)]
mod tests {
    use super::super::tool_view::{Role, Section};
    use super::*;
    use skyhook::agent::{ObservationSnapshot, ObservedEvent, RuntimeEvent};
    use skyhook::identity::SessionId;
    use skyhook::job::{JobRole, JobState};
    use skyhook::provider::protocol::{
        AssistantItem, BlockKind, ContentDelta, ItemKind, Message, ModelRequest, ReplayEnvelope,
        ResponseEvent, ToolCall, ToolResult, UserContent,
    };
    use skyhook::session::{EventRecord, ModelPurpose, SessionEvent};

    pub(super) fn root(seed: u8) -> AgentId {
        AgentId::root(SessionId::from_bytes([seed; 16]))
    }

    /// Append one journal record to a snapshot fixture and return its sequence.
    pub(super) fn record(
        snapshot: &mut ObservationSnapshot,
        agent: &AgentId,
        event: SessionEvent,
    ) -> u64 {
        let sequence = snapshot
            .records
            .last_key_value()
            .map_or(1, |(sequence, _)| sequence + 1);
        update(
            snapshot,
            RuntimeEvent::Record(Box::new(EventRecord {
                id: skyhook::identity::EventId::generate().unwrap(),
                queue_attempt: None,
                version: 1,
                sequence,
                timestamp_millis: sequence as i64 * 1000,
                agent: agent.clone(),
                event,
            })),
        );
        sequence
    }

    /// Apply one runtime event; deltas start their native item/block on first use.
    pub(super) fn update(snapshot: &mut ObservationSnapshot, event: RuntimeEvent) {
        if let RuntimeEvent::ResponseEvent {
            agent,
            request,
            event: ResponseEvent::BlockDelta { item, block, .. },
        } = &event
            && !snapshot
                .responses
                .get(&(agent.clone(), *request))
                .is_some_and(|live| live.snapshot().items.iter().any(|entry| entry.id == *item))
        {
            let (position, kind, block_kind) = if item == "reasoning" {
                (0, ItemKind::Reasoning, BlockKind::Reasoning)
            } else {
                (1, ItemKind::Text, BlockKind::Text)
            };
            for started in [
                ResponseEvent::ItemStarted {
                    id: item.clone(),
                    position,
                    kind,
                },
                ResponseEvent::BlockStarted {
                    item: item.clone(),
                    id: block.clone(),
                    position: 0,
                    kind: block_kind,
                },
            ] {
                response(snapshot, agent, *request, started);
            }
        }
        snapshot.apply(ObservedEvent {
            revision: snapshot.revision + 1,
            event,
        });
    }

    pub(super) fn response(
        snapshot: &mut ObservationSnapshot,
        agent: &AgentId,
        request: u64,
        event: ResponseEvent,
    ) {
        let agent = agent.clone();
        update(
            snapshot,
            RuntimeEvent::ResponseEvent {
                agent,
                request,
                event,
            },
        );
    }

    /// Stream `text` into the `text` (or `reasoning`) item of a request.
    pub(super) fn delta(
        snapshot: &mut ObservationSnapshot,
        agent: &AgentId,
        request: u64,
        item: &str,
        text: &str,
    ) {
        let event = ResponseEvent::BlockDelta {
            item: item.into(),
            block: format!("{item}:0"),
            delta: ContentDelta::Text(text.into()),
        };
        response(snapshot, agent, request, event);
    }

    pub(super) fn call_record(
        snapshot: &mut ObservationSnapshot,
        agent: &AgentId,
        id: &str,
    ) -> u64 {
        let args = serde_json::json!({"argv": ["echo", "  original\ttext\n"]});
        let call = ToolCall::new(id, "exec", args).unwrap();
        let message = Message::Assistant(vec![AssistantItem::tool_call("tool", 0, call)]);
        record(snapshot, agent, SessionEvent::MessageCommitted { message })
    }

    pub(super) fn result_record(
        snapshot: &mut ObservationSnapshot,
        agent: &AgentId,
        id: &str,
        error: bool,
    ) {
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
        record(snapshot, agent, SessionEvent::MessageCommitted { message });
    }

    /// Record a model request, creating a fresh model context unless one is given.
    pub(super) fn request(
        snapshot: &mut ObservationSnapshot,
        agent: &AgentId,
        context: Option<u64>,
    ) -> u64 {
        let template = ModelRequest {
            model: "fixture-model".into(),
            system: vec![],
            history: Vec::new(),
            tail: Vec::new(),
            history_lifetime: Default::default(),
            tools: vec![],
            response_schema: None,
            reasoning: None,
            max_output_tokens: Some(100),
            correlation: None,
            blobs: Default::default(),
        };
        let context = context.unwrap_or_else(|| {
            let provider = "fixture".into();
            record(
                snapshot,
                agent,
                SessionEvent::ModelContext { provider, template },
            )
        });
        let text = "original request".into();
        let message = Message::User(vec![UserContent::Text { text }]);
        let event = SessionEvent::ModelRequested {
            context,
            history: vec![],
            tail: vec![message],
            history_lifetime: Default::default(),
            purpose: ModelPurpose::Agent,
        };
        record(snapshot, agent, event)
    }

    pub(super) fn replay() -> ReplayEnvelope {
        ReplayEnvelope {
            version: 1,
            protocol: "fixture".into(),
            model: "fixture".into(),
            scope: "reasoning".into(),
            payload: serde_json::json!({"signature": "opaque"}),
            conversation_bound: false,
        }
    }

    pub(super) fn job_info(agent: &AgentId, id: u64, role: JobRole, state: JobState) -> JobInfo {
        JobInfo {
            id: JobId::new(id).unwrap(),
            agent: agent.clone(),
            name: None,
            tool: "exec".into(),
            role,
            args: serde_json::json!({"argv": ["echo"]}),
            parent: None,
            state,
            location: skyhook::execution::ExecutionLocation::named("root", "/workspace".into()),
            error: None,
        }
    }

    #[test]
    fn canonical_card_projection_matches_structured_plain_text_and_is_cached() {
        let header = vec![Run::new("▾ tool", Role::ToolName)];
        let line = Section::Line(vec![Run::new("  output\t ", Role::Plain)]);
        let body = Document {
            sections: vec![line.clone()],
        };
        let complete = Document {
            sections: vec![Section::Line(header.clone()), line],
        };
        let entry = Entry::card(EntryKey::Record(1), header.clone(), Some(body.clone()));
        assert_eq!(entry.text(), complete.plain_text());
        assert_eq!(entry.document(), Some(&body));
        assert_eq!(entry.header(), Some(header.as_slice()));
        let collapsed = Entry::card(EntryKey::Record(1), header, None);
        assert_eq!(collapsed.text(), "▾ tool");
        assert!(collapsed.document().is_none());
    }
}

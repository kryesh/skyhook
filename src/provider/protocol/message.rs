use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    media::{AttachmentRef, ImageRef},
    named_enum::named_enum,
    newtype::{Blank, nonblank, string_newtype},
};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "role", content = "content", rename_all = "snake_case")]
pub enum Message {
    User(Vec<UserContent>),
    Assistant(Vec<AssistantItem>),
    Tool(Vec<ToolResult>),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserContent {
    Text {
        text: String,
    },
    Attachment {
        attachment: AttachmentRef,
    },
    /// Text the harness produced rather than the user: it joins the final turn
    /// instead of posing as user input, and is never cached as history.
    Runtime {
        text: String,
    },
}

impl UserContent {
    #[must_use]
    pub fn is_image(&self) -> bool {
        matches!(
            self,
            Self::Attachment {
                attachment: AttachmentRef::Image(_)
            }
        )
    }

    /// Content the harness produced rather than the user or a parent.
    #[must_use]
    pub fn is_runtime(&self) -> bool {
        matches!(self, Self::Runtime { .. })
    }

    /// The text the model reads, or the attachment whose content is loaded separately.
    pub fn text(&self) -> Result<Cow<'_, str>, &AttachmentRef> {
        match self {
            Self::Text { text } | Self::Runtime { text } => Ok(Cow::Borrowed(text)),
            Self::Attachment { attachment } => Err(attachment),
        }
    }
}

string_newtype! {
    /// Nonblank provider item identity, unique within one response.
    #[derive(PartialOrd, Ord)]
    pub struct ItemId(Blank) = |id| nonblank("identity", id);
}

string_newtype! {
    /// Nonblank block identity, scoped to its item.
    #[derive(PartialOrd, Ord)]
    pub struct BlockId(Blank) = |id| nonblank("identity", id);
}

/// Provider ordering key. Not an index into anything.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Position(u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("position does not fit the journal")]
pub struct PositionOverflow;

impl Position {
    #[must_use]
    pub fn get(self) -> u32 {
        self.0
    }
}

impl From<u32> for Position {
    fn from(position: u32) -> Self {
        Self(position)
    }
}

impl TryFrom<usize> for Position {
    type Error = PositionOverflow;
    fn try_from(position: usize) -> Result<Self, PositionOverflow> {
        u32::try_from(position)
            .map(Self)
            .map_err(|_| PositionOverflow)
    }
}

/// A provider-native item. Its blocks and replay state must remain grouped.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AssistantItem {
    Text {
        id: ItemId,
        position: Position,
        blocks: Vec<TextBlock>,
    },
    Reasoning {
        id: ItemId,
        position: Position,
        blocks: Vec<TextBlock>,
        replay: Option<Replay>,
    },
    ToolCall {
        id: ItemId,
        position: Position,
        call: ToolCall,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TextBlock {
    pub id: BlockId,
    pub position: Position,
    pub text: String,
}

named_enum! {
    #[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
    pub enum ItemKind {
        Text = "text",
        Reasoning = "reasoning",
        ToolCall = "tool_call",
    }
}

/// The visible reply an assistant response projects: its text blocks concatenated,
/// with a whitespace-only projection normalized to none.
///
/// Blank text is content that must be kept for replay (see
/// [`Message::is_content_free`]), but it is not an answer. Providers routinely emit a
/// whitespace-only text block alongside tool calls — typically a `"\n\n"` separator
/// sent as `content` beside `reasoning_content`, a strictly empty delta already being
/// dropped by the Chat decoder — and a reasoning model does so on nearly every working
/// turn. Every consumer that asks "did this response say
/// anything?" must therefore normalize here rather than test `is_empty` on a raw
/// concatenation, or a child agent publishes a blank reply to its parent per turn.
#[must_use]
pub fn visible_text(items: &[AssistantItem]) -> String {
    let text: String = items
        .iter()
        .filter_map(AssistantItem::text_content)
        .collect();
    if text.trim().is_empty() {
        String::new()
    } else {
        text
    }
}

/// Opaque native replay state, attached exactly once to its owning item.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Replay {
    pub provenance: Provenance,
    pub payload: Value,
    pub binding: Binding,
}

/// Where a replay came from. Encoders compare it for equality and never interpret it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Provenance {
    pub protocol: String,
    pub model: String,
    pub scope: Scope,
}

string_newtype! {
    /// The configured provider identity a replay is valid under, assigned by the backend
    /// that issued it. Private state never crosses to another endpoint or provider name.
    pub struct Scope(Blank) = |scope| nonblank("replay scope", scope);
}

named_enum! {
    /// How tightly a replay is bound to the history that produced it.
    #[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
    pub enum Binding {
        /// Replays under any history with matching provenance.
        Free = "free",
        /// Signed against the exact preceding conversation; compaction or a mode switch
        /// invalidates it.
        Conversation = "conversation",
    }
}

impl AssistantItem {
    /// Single-block items with known-good identities, for fixtures and hosts.
    /// Decoders parse wire identities at their boundary instead.
    pub fn text(
        id: impl Into<String>,
        position: impl Into<Position>,
        text: impl Into<String>,
    ) -> Self {
        let (id, block) = Self::single_ids(id.into());
        Self::Text {
            id,
            position: position.into(),
            blocks: vec![TextBlock {
                id: block,
                position: Position(0),
                text: text.into(),
            }],
        }
    }

    pub fn reasoning(
        id: impl Into<String>,
        position: impl Into<Position>,
        text: impl Into<String>,
        replay: Option<Replay>,
    ) -> Self {
        let (id, block) = Self::single_ids(id.into());
        Self::Reasoning {
            id,
            position: position.into(),
            blocks: vec![TextBlock {
                id: block,
                position: Position(0),
                text: text.into(),
            }],
            replay,
        }
    }

    pub fn tool_call(id: impl Into<String>, position: impl Into<Position>, call: ToolCall) -> Self {
        let (id, _) = Self::single_ids(id.into());
        Self::ToolCall {
            id,
            position: position.into(),
            call,
        }
    }

    fn single_ids(id: String) -> (ItemId, BlockId) {
        let block = BlockId::try_from(format!("{id}:0")).expect("suffixed id is nonblank");
        let id = ItemId::try_from(id).expect("item ids are nonblank");
        (id, block)
    }

    #[must_use]
    pub fn id(&self) -> &ItemId {
        match self {
            Self::Text { id, .. } | Self::Reasoning { id, .. } | Self::ToolCall { id, .. } => id,
        }
    }

    #[must_use]
    pub fn position(&self) -> Position {
        match self {
            Self::Text { position, .. }
            | Self::Reasoning { position, .. }
            | Self::ToolCall { position, .. } => *position,
        }
    }

    #[must_use]
    pub fn kind(&self) -> ItemKind {
        match self {
            Self::Text { .. } => ItemKind::Text,
            Self::Reasoning { .. } => ItemKind::Reasoning,
            Self::ToolCall { .. } => ItemKind::ToolCall,
        }
    }

    #[must_use]
    pub fn replay(&self) -> Option<&Replay> {
        match self {
            Self::Reasoning { replay, .. } => replay.as_ref(),
            Self::Text { .. } | Self::ToolCall { .. } => None,
        }
    }

    #[must_use]
    pub fn call(&self) -> Option<&ToolCall> {
        match self {
            Self::ToolCall { call, .. } => Some(call),
            Self::Text { .. } | Self::Reasoning { .. } => None,
        }
    }

    /// Concatenates a text item's blocks; other kinds project no text.
    #[must_use]
    pub fn text_content(&self) -> Option<String> {
        match self {
            Self::Text { blocks, .. } => {
                Some(blocks.iter().map(|block| block.text.as_str()).collect())
            }
            Self::Reasoning { .. } | Self::ToolCall { .. } => None,
        }
    }

    /// Concatenates a reasoning item's readable blocks; other kinds project none.
    #[cfg(test)]
    pub fn reasoning_text(&self) -> Option<String> {
        match self {
            Self::Reasoning { blocks, .. } => {
                Some(blocks.iter().map(|block| block.text.as_str()).collect())
            }
            Self::Text { .. } | Self::ToolCall { .. } => None,
        }
    }

    /// Whether nothing of this item can be encoded: no blocks and no replay. A blank
    /// text block is content; a tool call always is.
    #[must_use]
    pub fn is_content_free(&self) -> bool {
        match self {
            Self::Text { blocks, .. } => blocks.is_empty(),
            Self::Reasoning { blocks, replay, .. } => blocks.is_empty() && replay.is_none(),
            Self::ToolCall { .. } => false,
        }
    }

    /// Drop replay bound to the conversation that produced it, keeping readable text.
    pub fn unbind(&mut self) {
        if let Self::Reasoning { replay, .. } = self
            && replay
                .as_ref()
                .is_some_and(|replay| replay.binding == Binding::Conversation)
        {
            *replay = None;
        }
    }
}

/// A completed call: identity is nonblank and arguments are a JSON object.
/// Fields are private (also through serde); provider name rules stay at their boundaries.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(try_from = "RawToolCall")]
pub struct ToolCall {
    id: String,
    name: String,
    arguments: Map<String, Value>,
}

/// Wire/journal DTO; its shape intentionally matches the original completed call.
#[derive(Deserialize)]
struct RawToolCall {
    id: String,
    name: String,
    arguments: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ToolCallError {
    #[error("tool call ID must be nonblank")]
    EmptyId,
    #[error("tool call name must be nonblank")]
    EmptyName,
    #[error("tool call arguments must be a JSON object")]
    ArgumentsNotObject,
}

impl ToolCall {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: Value,
    ) -> Result<Self, ToolCallError> {
        let id = id.into();
        let name = name.into();
        if id.trim().is_empty() {
            return Err(ToolCallError::EmptyId);
        }
        if name.trim().is_empty() {
            return Err(ToolCallError::EmptyName);
        }
        let Value::Object(arguments) = arguments else {
            return Err(ToolCallError::ArgumentsNotObject);
        };
        Ok(Self {
            id,
            name,
            arguments,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn arguments(&self) -> &Map<String, Value> {
        &self.arguments
    }
}

impl TryFrom<RawToolCall> for ToolCall {
    type Error = ToolCallError;

    fn try_from(raw: RawToolCall) -> Result<Self, Self::Error> {
        Self::new(raw.id, raw.name, raw.arguments)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolResult {
    pub call_id: String,
    pub name: String,
    pub result: Value,
    #[serde(default)]
    pub images: Vec<ImageRef>,
    #[serde(default)]
    pub is_error: bool,
}

#[cfg(test)]
mod visible_text_tests {
    use super::*;

    fn tool_call() -> AssistantItem {
        AssistantItem::tool_call(
            "call",
            1,
            ToolCall::new("call", "tool", serde_json::json!({})).unwrap(),
        )
    }

    /// The shape a reasoning model emits on a working turn: private reasoning, a
    /// blank text block, and the calls. That turn answered nothing, so it must
    /// project no visible text and stay distinct from a real reply.
    #[test]
    fn whitespace_only_response_projects_no_visible_text() {
        for blank in ["", "\n\n", " ", "\t\n ", "\r\n"] {
            let items = vec![
                AssistantItem::reasoning("thought", 0, "private", None),
                AssistantItem::text("blank", 1, blank),
                tool_call(),
            ];
            assert_eq!(visible_text(&items), "", "blank text {blank:?}");
            // Blank text remains content for replay; only the projection normalizes.
            assert!(!crate::session::Message::Assistant(items).is_content_free());
        }
    }

    /// Real text is projected byte for byte, including surrounding whitespace and
    /// block boundaries, so a child reply keeps matching the committed assistant text.
    #[test]
    fn substantive_text_is_projected_verbatim_across_blocks() {
        let block = |id: &str, position, text: &str| TextBlock {
            id: BlockId::try_from(id.to_owned()).unwrap(),
            position: Position(position),
            text: text.into(),
        };
        let items = vec![
            AssistantItem::reasoning("thought", 0, "private", None),
            AssistantItem::Text {
                id: ItemId::try_from("answer".to_owned()).unwrap(),
                position: Position(1),
                blocks: vec![block("a", 0, " first"), block("b", 1, "\nsecond ")],
            },
            tool_call(),
            AssistantItem::text("more", 3, "third"),
        ];
        assert_eq!(visible_text(&items), " first\nsecond third");
        assert_eq!(items[1].text_content().as_deref(), Some(" first\nsecond "));
        assert_eq!(items[0].reasoning_text().as_deref(), Some("private"));
        assert_eq!(items[0].text_content(), None);
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identities_and_positions_are_parsed_at_the_boundary() {
        assert_eq!(ItemId::try_from(" ".to_owned()), Err(Blank("identity")));
        assert_eq!(BlockId::try_from(String::new()), Err(Blank("identity")));
        assert_eq!(Position::try_from(usize::MAX), Err(PositionOverflow));
        let item: AssistantItem =
            serde_json::from_value(json!({"kind":"text", "id":"t", "position":3,
                "blocks":[{"id":"t:0", "position":0, "text":"hi"}]}))
            .unwrap();
        assert_eq!(item, AssistantItem::text("t", 3, "hi"));
        assert!(
            serde_json::from_value::<AssistantItem>(json!({"kind":"text", "id":"",
            "position":0, "blocks":[]}))
            .is_err()
        );
    }
}

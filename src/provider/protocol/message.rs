use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::media::{AttachmentRef, ImageRef};

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
    Text { text: String },
    Attachment { attachment: AttachmentRef },
    Runtime { text: String },
    ParentInput { text: String },
    Compaction { text: String },
}

impl Message {
    /// An assistant message that yields no content blocks at all, and so cannot be
    /// encoded into a later request: Anthropic rejects it outright, and Chat/Responses
    /// drop it silently. It must never reach the append-only journal, because a
    /// committed one makes every subsequent request fail.
    ///
    /// Emptiness here is structural, never a judgement about text. An empty or
    /// whitespace-only text block is content: providers legitimately emit one
    /// alongside tool calls on a non-final turn, and it replays without complaint.
    /// Reasoning is content whenever it carries replay state, or a block that an
    /// encoder may render.
    #[must_use]
    pub fn is_content_free(&self) -> bool {
        match self {
            Self::Assistant(items) => items.iter().all(AssistantItem::is_content_free),
            Self::User(_) | Self::Tool(_) => false,
        }
    }

    /// Drop replay bound to the conversation that produced it, keeping display text. Changing
    /// that conversation, as compaction or a mode switch does, invalidates such replay; other
    /// replay is kept. A message left content-free cannot be encoded and is dropped whole.
    #[must_use]
    pub fn without_bound_reasoning(mut self) -> Option<Self> {
        if let Self::Assistant(items) = &mut self {
            items.iter_mut().for_each(AssistantItem::unbind);
        }
        (!self.is_content_free()).then_some(self)
    }
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
}

/// Nonblank provider item identity, unique within one response.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(try_from = "String", into = "String")]
pub struct ItemId(String);

/// Nonblank block identity, scoped to its item.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(try_from = "String", into = "String")]
pub struct BlockId(String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("identity must not be blank")]
pub struct BlankId;

macro_rules! nonblank_id {
    ($($id:ident),+) => {$(
        impl TryFrom<String> for $id {
            type Error = BlankId;
            fn try_from(id: String) -> Result<Self, BlankId> {
                if id.trim().is_empty() {
                    return Err(BlankId);
                }
                Ok(Self(id))
            }
        }

        impl From<$id> for String {
            fn from(id: $id) -> Self {
                id.0
            }
        }

        impl $id {
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $id {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    )+};
}
nonblank_id!(ItemId, BlockId);

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

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Text,
    Reasoning,
    ToolCall,
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

/// The configured provider identity a replay is valid under, assigned by the backend
/// that issued it. Private state never crosses to another endpoint or provider name.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(try_from = "String", into = "String")]
pub struct Scope(String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("replay scope must not be blank")]
pub struct BlankScope;

impl TryFrom<String> for Scope {
    type Error = BlankScope;
    fn try_from(scope: String) -> Result<Self, BlankScope> {
        if scope.trim().is_empty() {
            return Err(BlankScope);
        }
        Ok(Self(scope))
    }
}

impl From<Scope> for String {
    fn from(scope: Scope) -> Self {
        scope.0
    }
}

impl Scope {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// How tightly a replay is bound to the history that produced it.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Binding {
    /// Replays under any history with matching provenance.
    Free,
    /// Signed against the exact preceding conversation; compaction or a mode switch
    /// invalidates it.
    Conversation,
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
mod content_free_tests {
    use super::*;
    use serde_json::json;

    fn envelope() -> Replay {
        Replay {
            provenance: Provenance {
                protocol: "anthropic".into(),
                model: "model".into(),
                scope: Scope::try_from("scope".to_owned()).unwrap(),
            },
            payload: json!({"type":"thinking","thinking":"private","signature":"signed"}),
            binding: Binding::Conversation,
        }
    }

    fn id(id: &str) -> ItemId {
        ItemId::try_from(id.to_owned()).unwrap()
    }

    /// Items whose blocks were all dropped, as a malformed decode would leave them.
    fn blockless_text() -> AssistantItem {
        AssistantItem::Text {
            id: id("text"),
            position: Position(0),
            blocks: Vec::new(),
        }
    }

    fn blockless_reasoning(replay: Option<Replay>) -> AssistantItem {
        AssistantItem::Reasoning {
            id: id("reasoning"),
            position: Position(0),
            blocks: Vec::new(),
            replay,
        }
    }

    #[test]
    fn content_free_means_no_blocks_at_all_not_empty_text() {
        // The only unencodable shapes: no items, or items carrying no blocks and
        // no replay state. A tool call is always content.
        assert!(Message::Assistant(Vec::new()).is_content_free());
        assert!(Message::Assistant(vec![blockless_text()]).is_content_free());
        assert!(Message::Assistant(vec![blockless_reasoning(None)]).is_content_free());
        // Only assistant messages can be content-free.
        assert!(!Message::User(Vec::new()).is_content_free());
        assert!(!Message::Tool(Vec::new()).is_content_free());
    }

    #[test]
    fn empty_and_whitespace_text_blocks_remain_content() {
        // Providers emit an empty or blank text block alongside tool calls on a
        // non-final turn. Such a block encodes and replays, so it is not a failure.
        for text in ["", " ", "\n\t "] {
            let item = AssistantItem::text("answer", 0, text);
            assert!(!Message::Assistant(vec![item]).is_content_free());
        }
        let call = ToolCall::new("call", "shell", json!({})).unwrap();
        let non_final = vec![
            AssistantItem::text("answer", 0, ""),
            AssistantItem::tool_call("tool-1", 1, call),
        ];
        assert!(!Message::Assistant(non_final).is_content_free());
        // Reasoning is content through a rendered block or through replay state.
        let blank_prose = AssistantItem::reasoning("thought", 0, "   ", None);
        assert!(!Message::Assistant(vec![blank_prose]).is_content_free());
        let signed = blockless_reasoning(Some(envelope()));
        assert!(!Message::Assistant(vec![signed]).is_content_free());
        let visible = AssistantItem::text("answer", 0, "hello");
        assert!(!Message::Assistant(vec![visible]).is_content_free());
    }

    #[test]
    fn unbinding_drops_only_conversation_bound_replay_and_empties_fall_away() {
        let free = Replay {
            binding: Binding::Free,
            ..envelope()
        };
        let message = Message::Assistant(vec![
            AssistantItem::reasoning("bound", 0, "visible", Some(envelope())),
            AssistantItem::reasoning("free", 1, "kept", Some(free.clone())),
        ]);
        let Some(Message::Assistant(items)) = message.without_bound_reasoning() else {
            panic!("readable text keeps the message")
        };
        assert_eq!(items[0].replay(), None);
        assert_eq!(items[1].replay(), Some(&free));
        let signed_only = Message::Assistant(vec![blockless_reasoning(Some(envelope()))]);
        assert_eq!(signed_only.without_bound_reasoning(), None);
    }

    #[test]
    fn identities_and_positions_are_parsed_at_the_boundary() {
        assert_eq!(ItemId::try_from(" ".to_owned()), Err(BlankId));
        assert_eq!(BlockId::try_from(String::new()), Err(BlankId));
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
            assert!(!Message::Assistant(items).is_content_free());
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

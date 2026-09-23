use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::provider::ProviderError;

use super::{AssistantItem, BlockId, ItemId, ItemKind, ToolCall};

/// One provider stream item. Deltas are display only; `End` carries the whole
/// response and supersedes every delta before it.
#[derive(Clone, Debug, PartialEq)]
pub enum ResponseEvent {
    /// Provisional text for one block. The first delta for a block opens it; arrival
    /// order is display order. Tool-call deltas carry argument JSON fragments.
    Delta {
        block: BlockRef,
        kind: ItemKind,
        text: String,
    },
    /// A cumulative snapshot, not an increment.
    Usage(Usage),
    /// The authoritative response. Nothing follows it.
    End(Completion),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlockRef {
    pub item: ItemId,
    pub block: BlockId,
}

/// A complete response: its items in position order and how it ended. Built only
/// through the constructors, which decide what a tool call may follow.
#[derive(Clone, Debug, PartialEq)]
pub struct Completion {
    items: Vec<AssistantItem>,
    outcome: Outcome,
}

/// How a response ended, as the journal records it.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Answer,
    ToolUse,
    Cut(CutReason),
}

/// Why a response ended before a normal finish. Retained text and reasoning stay in
/// the completion; tool calls never do.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CutReason {
    MaxTokens,
    Refusal,
    Aborted,
    /// The provider ended without a normal finish it could name.
    Incomplete,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CompletionError {
    #[error("tool use without any tool call")]
    NoCalls,
    #[error("tool calls on a response that does not authorize them")]
    UnauthorizedCalls,
    #[error("duplicate item id {0}")]
    DuplicateItem(ItemId),
    #[error("duplicate item position {0}")]
    DuplicatePosition(u32),
    #[error("duplicate tool call id {0}")]
    DuplicateCall(String),
    #[error("text item {0} has no blocks")]
    BlocklessText(ItemId),
}

impl From<CompletionError> for ProviderError {
    fn from(error: CompletionError) -> Self {
        Self::protocol(error.to_string())
    }
}

impl Completion {
    /// A normal finish without tool calls.
    pub fn answer(items: Vec<AssistantItem>) -> Result<Self, CompletionError> {
        Self::without_calls(items, Outcome::Answer)
    }

    /// A normal finish whose tool calls the runtime may execute.
    pub fn tool_use(items: Vec<AssistantItem>) -> Result<Self, CompletionError> {
        let items = Self::validated(items)?;
        if !items.iter().any(|item| item.call().is_some()) {
            return Err(CompletionError::NoCalls);
        }
        Ok(Self {
            items,
            outcome: Outcome::ToolUse,
        })
    }

    /// An abnormal end keeping whatever safe content the decoder retained.
    pub fn cut(items: Vec<AssistantItem>, reason: CutReason) -> Result<Self, CompletionError> {
        Self::without_calls(items, Outcome::Cut(reason))
    }

    /// A normal finish with calls when there are any, otherwise an answer.
    pub fn finished(items: Vec<AssistantItem>) -> Result<Self, CompletionError> {
        if items.iter().any(|item| item.call().is_some()) {
            Self::tool_use(items)
        } else {
            Self::answer(items)
        }
    }

    fn without_calls(items: Vec<AssistantItem>, outcome: Outcome) -> Result<Self, CompletionError> {
        let items = Self::validated(items)?;
        if items.iter().any(|item| item.call().is_some()) {
            return Err(CompletionError::UnauthorizedCalls);
        }
        Ok(Self { items, outcome })
    }

    fn validated(mut items: Vec<AssistantItem>) -> Result<Vec<AssistantItem>, CompletionError> {
        let mut ids = BTreeSet::new();
        let mut positions = BTreeSet::new();
        let mut calls = BTreeSet::new();
        for item in &items {
            if !ids.insert(item.id().clone()) {
                return Err(CompletionError::DuplicateItem(item.id().clone()));
            }
            if !positions.insert(item.position()) {
                return Err(CompletionError::DuplicatePosition(item.position().get()));
            }
            match item {
                AssistantItem::Text { id, blocks, .. } if blocks.is_empty() => {
                    return Err(CompletionError::BlocklessText(id.clone()));
                }
                AssistantItem::ToolCall { call, .. } if !calls.insert(call.id().to_owned()) => {
                    return Err(CompletionError::DuplicateCall(call.id().to_owned()));
                }
                _ => {}
            }
        }
        items.sort_by_key(|item| item.position());
        Ok(items)
    }

    #[must_use]
    pub fn outcome(&self) -> Outcome {
        self.outcome
    }

    #[must_use]
    pub fn items(&self) -> &[AssistantItem] {
        &self.items
    }

    #[must_use]
    pub fn into_items(self) -> Vec<AssistantItem> {
        self.items
    }

    /// Nonempty exactly when the outcome is `ToolUse`.
    pub fn calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.items.iter().filter_map(AssistantItem::call)
    }

    /// The readable blocks in position order, keyed like their deltas.
    fn blocks(&self) -> Vec<LiveBlock> {
        self.items
            .iter()
            .flat_map(|item| {
                let (blocks, kind) = match item {
                    AssistantItem::Text { blocks, .. } => (blocks.as_slice(), ItemKind::Text),
                    AssistantItem::Reasoning { blocks, .. } => {
                        (blocks.as_slice(), ItemKind::Reasoning)
                    }
                    AssistantItem::ToolCall { .. } => (&[][..], ItemKind::ToolCall),
                };
                blocks.iter().map(move |block| LiveBlock {
                    block: BlockRef {
                        item: item.id().clone(),
                        block: block.id.clone(),
                    },
                    kind,
                    text: block.text.clone(),
                })
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Usage {
    /// Non-cached input tokens (including cache creation). Total prompt tokens
    /// are `input_tokens + cached_input_tokens`, as used by the runtime meter.
    pub input_tokens: u64,
    /// Cache-read input tokens, excluded from `input_tokens`.
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    pub fn accumulate(&mut self, usage: Self) {
        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(usage.cached_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
    }
}

/// The arrival-ordered provisional view of an in-flight response, shared by the
/// runtime and observers. Consuming `push` makes "an event after the end"
/// unrepresentable.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LiveResponse {
    blocks: Vec<LiveBlock>,
    usage: Usage,
    /// The block the latest delta extended.
    current: Option<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LiveBlock {
    pub block: BlockRef,
    pub kind: ItemKind,
    pub text: String,
}

pub enum Step {
    Open(LiveResponse),
    Ended {
        completion: Completion,
        usage: Usage,
        /// The authoritative text in position order, keyed like the deltas were.
        blocks: Vec<LiveBlock>,
    },
}

impl LiveResponse {
    #[must_use]
    pub fn push(mut self, event: ResponseEvent) -> Step {
        match event {
            ResponseEvent::Delta { block, kind, text } => {
                let index = match self.blocks.iter().position(|live| live.block == block) {
                    Some(index) => index,
                    None => {
                        self.blocks.push(LiveBlock {
                            block,
                            kind,
                            text: String::new(),
                        });
                        self.blocks.len() - 1
                    }
                };
                self.blocks[index].text.push_str(&text);
                self.current = Some(index);
                Step::Open(self)
            }
            ResponseEvent::Usage(usage) => {
                self.usage = usage;
                Step::Open(self)
            }
            ResponseEvent::End(completion) => Step::Ended {
                blocks: completion.blocks(),
                completion,
                usage: self.usage,
            },
        }
    }

    #[must_use]
    pub fn blocks(&self) -> &[LiveBlock] {
        &self.blocks
    }

    #[must_use]
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// Whether any content has streamed; usage alone is not content.
    #[must_use]
    pub fn saw_content(&self) -> bool {
        !self.blocks.is_empty()
    }

    /// The block still being streamed: the one the latest delta extended.
    #[must_use]
    pub fn current(&self) -> Option<&BlockRef> {
        self.current.map(|index| &self.blocks[index].block)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(id: &str) -> ToolCall {
        ToolCall::new(id, "run", json!({})).unwrap()
    }

    fn block(item: &str, block: &str) -> BlockRef {
        BlockRef {
            item: ItemId::try_from(item.to_owned()).unwrap(),
            block: BlockId::try_from(block.to_owned()).unwrap(),
        }
    }

    fn delta(item: &str, block_id: &str, kind: ItemKind, text: &str) -> ResponseEvent {
        ResponseEvent::Delta {
            block: block(item, block_id),
            kind,
            text: text.into(),
        }
    }

    #[test]
    fn constructors_decide_which_calls_may_follow() {
        let text = || AssistantItem::text("t", 0, "answer");
        let tool = || AssistantItem::tool_call("c", 1, call("call"));
        assert_eq!(
            Completion::answer(vec![text(), tool()]).unwrap_err(),
            CompletionError::UnauthorizedCalls
        );
        assert_eq!(
            Completion::cut(vec![tool()], CutReason::MaxTokens).unwrap_err(),
            CompletionError::UnauthorizedCalls
        );
        assert_eq!(
            Completion::tool_use(vec![text()]).unwrap_err(),
            CompletionError::NoCalls
        );
        let finished = Completion::finished(vec![text(), tool()]).unwrap();
        assert_eq!(finished.outcome(), Outcome::ToolUse);
        assert_eq!(finished.calls().count(), 1);
        let answered = Completion::finished(vec![text()]).unwrap();
        assert_eq!(
            (answered.outcome(), answered.calls().count()),
            (Outcome::Answer, 0)
        );
        let cut = Completion::cut(vec![text()], CutReason::Refusal).unwrap();
        assert_eq!(cut.outcome(), Outcome::Cut(CutReason::Refusal));
        assert!(Completion::answer(Vec::new()).unwrap().items().is_empty());
    }

    #[test]
    fn items_are_unique_by_id_position_and_call_and_ordered_by_position() {
        let later = AssistantItem::text("later", 7, "B");
        let earlier = AssistantItem::text("earlier", 2, "A");
        let ordered = Completion::answer(vec![later, earlier]).unwrap();
        let ids: Vec<_> = ordered
            .items()
            .iter()
            .map(|item| item.id().as_str())
            .collect();
        assert_eq!(ids, ["earlier", "later"]);
        let same_id = vec![
            AssistantItem::text("i", 0, "a"),
            AssistantItem::text("i", 1, "b"),
        ];
        assert!(matches!(
            Completion::answer(same_id).unwrap_err(),
            CompletionError::DuplicateItem(_)
        ));
        let same_position = vec![
            AssistantItem::text("a", 0, "a"),
            AssistantItem::text("b", 0, "b"),
        ];
        assert_eq!(
            Completion::answer(same_position).unwrap_err(),
            CompletionError::DuplicatePosition(0)
        );
        let same_call = vec![
            AssistantItem::tool_call("first", 0, call("same")),
            AssistantItem::tool_call("second", 1, call("same")),
        ];
        assert_eq!(
            Completion::tool_use(same_call).unwrap_err(),
            CompletionError::DuplicateCall("same".into())
        );
        let blockless = AssistantItem::Text {
            id: ItemId::try_from("empty".to_owned()).unwrap(),
            position: 0.into(),
            blocks: Vec::new(),
        };
        assert!(matches!(
            Completion::answer(vec![blockless]).unwrap_err(),
            CompletionError::BlocklessText(_)
        ));
        // Reasoning may be replay-only; a blank text block is content.
        let replay_only = AssistantItem::Reasoning {
            id: ItemId::try_from("r".to_owned()).unwrap(),
            position: 0.into(),
            blocks: Vec::new(),
            replay: None,
        };
        let blank = AssistantItem::text("blank", 1, "");
        assert!(Completion::answer(vec![replay_only, blank]).is_ok());
    }

    #[test]
    fn live_view_keeps_arrival_order_tracks_the_current_block_and_replaces_usage() {
        let usage = |input_tokens, cached_input_tokens, output_tokens| Usage {
            input_tokens,
            cached_input_tokens,
            output_tokens,
        };
        let live = LiveResponse::default();
        assert!(!live.saw_content() && live.current().is_none());
        let Step::Open(live) = live.push(ResponseEvent::Usage(usage(100, 50, 3))) else {
            panic!("usage keeps the response open")
        };
        assert!(!live.saw_content());
        let events = [
            delta("later", "b", ItemKind::Text, "B"),
            delta("reason", "s", ItemKind::Reasoning, "think"),
            delta("later", "b", ItemKind::Text, "?"),
            ResponseEvent::Usage(usage(10, 5, 6)),
        ];
        let live = events
            .into_iter()
            .fold(live, |live, event| match live.push(event) {
                Step::Open(live) => live,
                Step::Ended { .. } => panic!("deltas keep the response open"),
            });
        assert!(live.saw_content());
        assert_eq!(live.current(), Some(&block("later", "b")));
        let texts: Vec<_> = live
            .blocks()
            .iter()
            .map(|live| (live.kind, live.text.as_str()))
            .collect();
        assert_eq!(
            texts,
            [(ItemKind::Text, "B?"), (ItemKind::Reasoning, "think")]
        );
        assert_eq!(live.usage(), usage(10, 5, 6));
        let completion = Completion::answer(vec![AssistantItem::text("later", 0, "B?")]).unwrap();
        let Step::Ended {
            completion: ended,
            usage: final_usage,
            blocks,
        } = live.push(ResponseEvent::End(completion.clone()))
        else {
            panic!("end closes the response")
        };
        assert_eq!((ended, final_usage), (completion, usage(10, 5, 6)));
        let final_texts: Vec<_> = blocks.iter().map(|live| live.text.as_str()).collect();
        assert_eq!(final_texts, ["B?"]);
        assert_eq!(blocks[0].block, block("later", "later:0"));
    }
}

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::provider::{ProviderError, ProviderErrorKind};

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

impl ResponseEvent {
    /// The event, with a usage snapshot checked by [`Usage::check`].
    pub fn checked(self) -> Result<Self, UsageError> {
        match self {
            Self::Usage(usage) => usage.check().map(Self::Usage),
            event => Ok(event),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlockRef {
    pub item: ItemId,
    pub block: BlockId,
}

impl BlockRef {
    /// The one block of a single-block item, as its constructors and the
    /// index-addressed decoders key it.
    pub fn single(item: impl std::fmt::Display) -> Self {
        Self {
            block: BlockId::try_from(format!("{item}:0")).expect("suffixed id is nonblank"),
            item: ItemId::try_from(item.to_string()).expect("item ids are nonblank"),
        }
    }
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
        ProviderErrorKind::Protocol.error(error.to_string())
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

    pub fn outcome(&self) -> Outcome {
        self.outcome
    }

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

/// A response's cumulative token counts as its provider reported them; a count the
/// provider did not report is absent, not zero.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Usage {
    /// Non-cached input tokens (including cache creation).
    pub input_tokens: Option<u64>,
    /// Cache-read input tokens, excluded from `input_tokens`.
    pub cached_input_tokens: Option<u64>,
    /// The part of `input_tokens` written to the provider's cache.
    pub cache_write_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// Token counts summed over usage reports, each missing count adding nothing.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub output_tokens: u64,
}

impl UsageTotals {
    pub fn add(&mut self, usage: Usage) {
        let add = |total: &mut u64, count: Option<u64>| {
            *total = total.saturating_add(count.unwrap_or_default());
        };
        add(&mut self.input_tokens, usage.input_tokens);
        add(&mut self.cached_input_tokens, usage.cached_input_tokens);
        add(
            &mut self.cache_write_input_tokens,
            usage.cache_write_input_tokens,
        );
        add(&mut self.output_tokens, usage.output_tokens);
    }

    pub fn merge(&mut self, totals: Self) {
        self.input_tokens = self.input_tokens.saturating_add(totals.input_tokens);
        self.cached_input_tokens =
            (self.cached_input_tokens).saturating_add(totals.cached_input_tokens);
        self.cache_write_input_tokens =
            (self.cache_write_input_tokens).saturating_add(totals.cache_write_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(totals.output_tokens);
    }
}

/// A usage report claiming more cache writes than the input they are part of.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("usage reports {cache_writes} cache-write tokens, more than its {input} input tokens")]
pub struct UsageError {
    pub cache_writes: u64,
    pub input: u64,
}

impl From<UsageError> for ProviderError {
    fn from(error: UsageError) -> Self {
        ProviderErrorKind::Protocol.error(error.to_string())
    }
}

impl Usage {
    /// A report of every count.
    #[must_use]
    pub const fn prompted(input: u64, cached: u64, written: u64, output: u64) -> Self {
        Self {
            input_tokens: Some(input),
            cached_input_tokens: Some(cached),
            cache_write_input_tokens: Some(written),
            output_tokens: Some(output),
        }
    }

    /// The whole prompt, cached or not. Unknown without the input count; missing cache
    /// reads add nothing.
    pub fn prompt_tokens(self) -> Option<u64> {
        let cached = self.cached_input_tokens.unwrap_or_default();
        Some(self.input_tokens?.saturating_add(cached))
    }

    /// What the request and its response occupy of the context: the whole prompt and
    /// the output, when the prompt is known.
    pub fn occupancy(self) -> Option<u64> {
        let output = self.output_tokens.unwrap_or_default();
        Some(self.prompt_tokens()?.saturating_add(output))
    }

    /// Cache writes are part of input, so a report claiming more is invalid.
    pub fn check(self) -> Result<Self, UsageError> {
        match (self.cache_write_input_tokens, self.input_tokens) {
            (Some(cache_writes), Some(input)) if cache_writes > input => Err(UsageError {
                cache_writes,
                input,
            }),
            _ => Ok(self),
        }
    }

    /// Add `usage`'s counts; a count only one side reported adds as it is.
    pub fn accumulate(&mut self, usage: Self) {
        let add = |count: &mut Option<u64>, other: Option<u64>| {
            *count = match (*count, other) {
                (Some(count), Some(other)) => Some(count.saturating_add(other)),
                (count, other) => count.or(other),
            };
        };
        add(&mut self.input_tokens, usage.input_tokens);
        add(&mut self.cached_input_tokens, usage.cached_input_tokens);
        add(
            &mut self.cache_write_input_tokens,
            usage.cache_write_input_tokens,
        );
        add(&mut self.output_tokens, usage.output_tokens);
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
                let index = self
                    .current
                    .filter(|&index| self.blocks[index].block == block)
                    .or_else(|| self.blocks.iter().position(|live| live.block == block));
                let index = match index {
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

    pub fn blocks(&self) -> &[LiveBlock] {
        &self.blocks
    }

    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// The block still being streamed: the one the latest delta extended.
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
    fn cache_writes_may_equal_input_but_never_exceed_it() {
        let valid = Usage::prompted(100, 0, 100, 0);
        assert_eq!(
            ResponseEvent::Usage(valid).checked(),
            Ok(ResponseEvent::Usage(valid))
        );
        let invalid = Usage::prompted(100, 0, 101, 0);
        let error = ResponseEvent::Usage(invalid).checked().unwrap_err();
        assert_eq!(
            ProviderError::from(error).kind(),
            crate::provider::ProviderErrorKind::Protocol
        );
    }

    #[test]
    fn live_view_keeps_arrival_order_tracks_the_current_block_and_replaces_usage() {
        let usage = |input, cached, output| Usage::prompted(input, cached, 0, output);
        let live = LiveResponse::default();
        assert!(live.blocks().is_empty() && live.current().is_none());
        let Step::Open(live) = live.push(ResponseEvent::Usage(usage(100, 50, 3))) else {
            panic!("usage keeps the response open")
        };
        assert!(live.blocks().is_empty());
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

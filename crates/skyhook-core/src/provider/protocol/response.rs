use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::provider::ProviderError;

use super::{AssistantBlock, AssistantItem, BlockContent, BlockKind, ItemKind, ReplayEnvelope};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub enum ResponseEvent {
    ItemStarted {
        id: String,
        position: usize,
        kind: ItemKind,
    },
    BlockStarted {
        item: String,
        id: String,
        position: usize,
        kind: BlockKind,
    },
    BlockDelta {
        item: String,
        block: String,
        delta: ContentDelta,
    },
    /// Complete authoritative content, replacing any provisional deltas.
    BlockEnded {
        item: String,
        block: String,
        content: BlockContent,
    },
    ItemEnded {
        id: String,
        replay: Option<ReplayEnvelope>,
    },
    /// Enrich opaque replay after item completion, before response completion.
    ItemReplayUpdated {
        id: String,
        replay: ReplayEnvelope,
    },
    /// Remove an unsafe/provisional item without promoting its partial content.
    ItemDiscarded {
        id: String,
    },
    /// A cumulative snapshot, not an increment.
    UsageUpdated {
        usage: Usage,
    },
    ResponseEnded {
        stop_reason: StopReason,
    },
}

pub type ResponseChunk = ResponseEvent;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub enum ContentDelta {
    Text(String),
    JsonFragment(String),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    ContentFilter,
    Aborted,
    Other(String),
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
    pub(crate) fn accumulate(&mut self, usage: Self) {
        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(usage.cached_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ResponseSnapshot {
    pub items: Vec<ItemSnapshot>,
    pub usage: Usage,
    pub stop_reason: Option<StopReason>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ItemSnapshot {
    pub id: String,
    pub position: usize,
    pub kind: ItemKind,
    pub blocks: Vec<BlockSnapshot>,
    pub replay: Option<ReplayEnvelope>,
    pub ended: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct BlockSnapshot {
    pub id: String,
    pub position: usize,
    pub kind: BlockKind,
    /// Provisional text/JSON until block end; authoritative text/JSON thereafter.
    pub text: String,
    pub content: Option<BlockContent>,
    pub ended: bool,
}

/// Strict item/block lifecycle reducer. IDs are stable identities; positions are
/// provider ordering keys (not arrival order). Block IDs are scoped to an item.
/// Rejected events do not mutate the reducer.
#[derive(Clone, Debug, Default)]
pub struct ResponseAssembler {
    items: BTreeMap<usize, ItemSnapshot>,
    discarded: BTreeSet<String>,
    usage: Usage,
    stop_reason: Option<StopReason>,
}

impl ResponseAssembler {
    pub fn usage(&self) -> Usage {
        self.usage
    }

    pub fn truncated(&self) -> bool {
        self.stop_reason == Some(StopReason::MaxTokens)
    }

    pub fn snapshot(&self) -> ResponseSnapshot {
        ResponseSnapshot {
            items: self.items.values().cloned().collect(),
            usage: self.usage,
            stop_reason: self.stop_reason.clone(),
        }
    }

    pub fn push(&mut self, event: &ResponseEvent) -> Result<(), ProviderError> {
        if self.stop_reason.is_some() {
            return Err(ProviderError::protocol("event after response ended"));
        }
        match event {
            ResponseEvent::ItemStarted { id, position, kind } => {
                if id.trim().is_empty()
                    || self.discarded.contains(id)
                    || self.items.values().any(|item| item.id == *id)
                {
                    return Err(ProviderError::protocol("empty or duplicate item ID"));
                }
                if self.items.contains_key(position) {
                    return Err(ProviderError::protocol("duplicate item position"));
                }
                self.items.insert(
                    *position,
                    ItemSnapshot {
                        id: id.clone(),
                        position: *position,
                        kind: *kind,
                        blocks: Vec::new(),
                        replay: None,
                        ended: false,
                    },
                );
            }
            ResponseEvent::BlockStarted {
                item,
                id,
                position,
                kind,
            } => {
                let item = self.pending_item(item)?;
                if id.trim().is_empty() || item.blocks.iter().any(|block| block.id == *id) {
                    return Err(ProviderError::protocol("empty or duplicate block ID"));
                }
                if item.blocks.iter().any(|block| block.position == *position) {
                    return Err(ProviderError::protocol("duplicate block position"));
                }
                let expected = match item.kind {
                    ItemKind::Text => BlockKind::Text,
                    ItemKind::Reasoning => BlockKind::Reasoning,
                    ItemKind::ToolCall => BlockKind::ToolCallArguments,
                };
                if *kind != expected {
                    return Err(ProviderError::protocol(
                        "block kind does not match item kind",
                    ));
                }
                item.blocks.push(BlockSnapshot {
                    id: id.clone(),
                    position: *position,
                    kind: *kind,
                    text: String::new(),
                    content: None,
                    ended: false,
                });
                item.blocks.sort_by_key(|block| block.position);
            }
            ResponseEvent::BlockDelta { item, block, delta } => {
                let pending = self.pending_block(item, block)?;
                let text = match (pending.kind, delta) {
                    (BlockKind::Text | BlockKind::Reasoning, ContentDelta::Text(text)) => text,
                    (BlockKind::ToolCallArguments, ContentDelta::JsonFragment(text)) => text,
                    _ => {
                        return Err(ProviderError::protocol(
                            "delta kind does not match block kind",
                        ));
                    }
                };
                pending.text.push_str(text);
            }
            ResponseEvent::BlockEnded {
                item,
                block,
                content,
            } => {
                if let BlockContent::ToolCall(call) = content
                    && self
                        .items
                        .values()
                        .flat_map(|item| &item.blocks)
                        .any(|block| {
                            block
                                .content
                                .as_ref()
                                .and_then(BlockContent::tool_call_ref)
                                .is_some_and(|previous| previous.id() == call.id())
                        })
                {
                    return Err(ProviderError::protocol("duplicate tool call ID"));
                }
                let pending = self.pending_block(item, block)?;
                if pending.kind != content.kind() {
                    return Err(ProviderError::protocol(
                        "terminal content kind does not match block kind",
                    ));
                }
                let text = match content {
                    BlockContent::Text { text } | BlockContent::Reasoning { text } => text.clone(),
                    BlockContent::ToolCall(call) => serde_json::to_string(call.arguments())
                        .expect("JSON object serialization cannot fail"),
                };
                pending.text = text;
                pending.content = Some(content.clone());
                pending.ended = true;
            }
            ResponseEvent::ItemEnded { id, replay } => {
                let item = self.pending_item(id)?;
                if item.blocks.iter().any(|block| !block.ended) {
                    return Err(ProviderError::protocol(
                        "item ended before all blocks ended",
                    ));
                }
                // Reasoning may be empty or consist solely of opaque replay state.
                if item.blocks.is_empty() && item.kind != ItemKind::Reasoning {
                    return Err(ProviderError::protocol("non-reasoning item has no blocks"));
                }
                if item.kind == ItemKind::ToolCall && item.blocks.len() != 1 {
                    return Err(ProviderError::protocol(
                        "tool call item must have exactly one arguments block",
                    ));
                }
                // Replay is opaque and may be attached to any item kind; native
                // codecs own protocol/model/scope compatibility checks.
                item.replay = replay.clone();
                item.ended = true;
            }
            ResponseEvent::ItemReplayUpdated { id, replay } => {
                let item = self
                    .items
                    .values_mut()
                    .find(|item| item.id == *id)
                    .filter(|item| item.ended)
                    .ok_or_else(|| {
                        ProviderError::protocol("replay update requires a completed item")
                    })?;
                item.replay = Some(replay.clone());
            }
            ResponseEvent::ItemDiscarded { id } => {
                let position = self
                    .items
                    .iter()
                    .find_map(|(position, item)| (item.id == *id).then_some(*position))
                    .ok_or_else(|| ProviderError::protocol("discarded item has not started"))?;
                self.items.remove(&position);
                self.discarded.insert(id.clone());
            }
            ResponseEvent::UsageUpdated { usage } => self.usage = *usage,
            ResponseEvent::ResponseEnded { stop_reason } => {
                self.validate_closed()?;
                self.stop_reason = Some(stop_reason.clone());
            }
        }
        Ok(())
    }

    fn pending_item(&mut self, id: &str) -> Result<&mut ItemSnapshot, ProviderError> {
        let item = self
            .items
            .values_mut()
            .find(|item| item.id == id)
            .ok_or_else(|| ProviderError::protocol(format!("item {id} has not started")))?;
        if item.ended {
            return Err(ProviderError::protocol(format!(
                "event after item {id} ended"
            )));
        }
        Ok(item)
    }

    fn pending_block(
        &mut self,
        item: &str,
        block: &str,
    ) -> Result<&mut BlockSnapshot, ProviderError> {
        let item = self.pending_item(item)?;
        let pending = item
            .blocks
            .iter_mut()
            .find(|pending| pending.id == block)
            .ok_or_else(|| ProviderError::protocol(format!("block {block} has not started")))?;
        if pending.ended {
            return Err(ProviderError::protocol(format!(
                "event after block {block} ended"
            )));
        }
        Ok(pending)
    }

    fn validate_closed(&self) -> Result<(), ProviderError> {
        if self.items.values().any(|item| !item.ended) {
            return Err(ProviderError::protocol(
                "response ended before all items ended",
            ));
        }
        Ok(())
    }

    /// Complete the response into validated conversation items.
    pub fn finish(self) -> Result<(Vec<AssistantItem>, Usage, StopReason), ProviderError> {
        self.validate_closed()?;
        let stop_reason = self
            .stop_reason
            .ok_or_else(|| ProviderError::protocol("response has no terminal stop reason"))?;
        let items = self
            .items
            .into_values()
            .map(|item| AssistantItem {
                id: item.id,
                position: item.position,
                kind: item.kind,
                blocks: item
                    .blocks
                    .into_iter()
                    .map(|block| AssistantBlock {
                        id: block.id,
                        position: block.position,
                        content: block
                            .content
                            .expect("ended blocks have authoritative content"),
                    })
                    .collect(),
                replay: item.replay,
            })
            .collect();
        Ok((items, self.usage, stop_reason))
    }
}

/// Emit complete final-only lifecycles while preserving item/block identities,
/// order and replay envelopes. Usage and response termination belong to callers.
#[cfg(test)]
pub fn events_for_content(items: &[AssistantItem]) -> Vec<ResponseEvent> {
    let mut events = Vec::new();
    for item in items {
        events.push(ResponseEvent::ItemStarted {
            id: item.id.clone(),
            position: item.position,
            kind: item.kind,
        });
        for block in &item.blocks {
            events.push(ResponseEvent::BlockStarted {
                item: item.id.clone(),
                id: block.id.clone(),
                position: block.position,
                kind: block.content.kind(),
            });
            events.push(ResponseEvent::BlockEnded {
                item: item.id.clone(),
                block: block.id.clone(),
                content: block.content.clone(),
            });
        }
        events.push(ResponseEvent::ItemEnded {
            id: item.id.clone(),
            replay: item.replay.clone(),
        });
    }
    events
}

#[cfg(test)]
mod tests {
    use super::super::{ToolCall, replay};
    use super::*;
    use serde_json::json;

    fn start(id: &str, position: usize, kind: ItemKind) -> ResponseEvent {
        ResponseEvent::ItemStarted {
            id: id.into(),
            position,
            kind,
        }
    }

    fn block(item: &str, id: &str, position: usize, kind: BlockKind) -> ResponseEvent {
        ResponseEvent::BlockStarted {
            item: item.into(),
            id: id.into(),
            position,
            kind,
        }
    }

    fn delta(item: &str, block: &str, delta: ContentDelta) -> ResponseEvent {
        ResponseEvent::BlockDelta {
            item: item.into(),
            block: block.into(),
            delta,
        }
    }

    fn end_block(item: &str, block: &str, content: BlockContent) -> ResponseEvent {
        ResponseEvent::BlockEnded {
            item: item.into(),
            block: block.into(),
            content,
        }
    }

    fn end_item(id: &str) -> ResponseEvent {
        ResponseEvent::ItemEnded {
            id: id.into(),
            replay: None,
        }
    }

    fn end_with(stop_reason: StopReason) -> ResponseEvent {
        ResponseEvent::ResponseEnded { stop_reason }
    }

    fn end() -> ResponseEvent {
        end_with(StopReason::EndTurn)
    }

    fn discard(id: &str) -> ResponseEvent {
        ResponseEvent::ItemDiscarded { id: id.into() }
    }

    fn text(text: &str) -> BlockContent {
        BlockContent::Text { text: text.into() }
    }

    fn assembled(events: impl IntoIterator<Item = ResponseEvent>) -> ResponseAssembler {
        let mut assembler = ResponseAssembler::default();
        for event in events {
            assembler.push(&event).unwrap();
        }
        assembler
    }

    #[test]
    fn discarded_partial_tools_cannot_reappear_or_execute() {
        let mut assembler = assembled([
            start("tool", 0, ItemKind::ToolCall),
            block("tool", "args", 0, BlockKind::ToolCallArguments),
            delta(
                "tool",
                "args",
                ContentDelta::JsonFragment("{\"incomplete\":".into()),
            ),
            discard("tool"),
        ]);
        assert!(
            assembler
                .push(&start("tool", 0, ItemKind::ToolCall))
                .is_err()
        );
        assembler.push(&end_with(StopReason::Aborted)).unwrap();
        let (items, _, reason) = assembler.finish().unwrap();
        assert!(items.is_empty());
        assert_eq!(reason, StopReason::Aborted);
    }

    #[test]
    fn replay_enrichment_requires_completed_item_and_preserves_content() {
        let replay = replay();
        let update = ResponseEvent::ItemReplayUpdated {
            id: "reason".into(),
            replay: replay.clone(),
        };
        let mut assembler = assembled([start("reason", 0, ItemKind::Reasoning)]);
        assert!(assembler.push(&update).is_err());
        for event in [end_item("reason"), update.clone(), end()] {
            assembler.push(&event).unwrap();
        }
        assert!(assembler.push(&update).is_err());
        let (items, _, _) = assembler.finish().unwrap();
        assert_eq!(items[0].replay, Some(replay));
    }

    #[test]
    fn interleaving_orders_items_and_blocks_by_position_not_arrival() {
        let assembler = assembled([
            start("later", 7, ItemKind::Text),
            start("earlier", 2, ItemKind::Text),
            block("later", "b", 8, BlockKind::Text),
            block("earlier", "b", 0, BlockKind::Text),
            block("later", "a", 3, BlockKind::Text),
            delta("later", "b", ContentDelta::Text("B?".into())),
            end_block("earlier", "b", text("early")),
            end_block("later", "a", text("A")),
            end_item("earlier"),
            end_block("later", "b", text("B")),
            end_item("later"),
            end(),
        ]);
        let (items, _, _) = assembler.finish().unwrap();
        let ids: Vec<_> = items.iter().map(|item| item.id.as_str()).collect();
        assert_eq!(ids, ["earlier", "later"]);
        let blocks: Vec<_> = items[1]
            .blocks
            .iter()
            .map(|block| block.id.as_str())
            .collect();
        assert_eq!(blocks, ["a", "b"]);
        assert_eq!(items[1].text_content().as_deref(), Some("AB"));
    }

    #[test]
    fn final_only_and_authoritative_replacement_with_usage_snapshots() {
        let mut assembler = assembled([
            start("i", 0, ItemKind::Text),
            block("i", "b", 0, BlockKind::Text),
            delta("i", "b", ContentDelta::Text("draft".into())),
        ]);
        let partial = assembler.snapshot();
        let draft = &partial.items[0].blocks[0];
        assert_eq!(
            (draft.text.as_str(), &draft.content, draft.ended),
            ("draft", &None, false)
        );
        assembler.push(&end_block("i", "b", text(""))).unwrap();
        assert_eq!(assembler.snapshot().items[0].blocks[0].text, "");
        assert_eq!(partial.items[0].blocks[0].text, "draft");
        assembler.push(&end_item("i")).unwrap();
        let call = ToolCall::new("call", "run", json!({"a": 1})).unwrap();
        let tool = AssistantItem::tool_call("tool", 1, call.clone());
        for event in events_for_content(std::slice::from_ref(&tool)) {
            assembler.push(&event).unwrap();
        }
        assert_eq!(tool.tool_call_ref(), Some(&call));
        let usage = |input_tokens, cached_input_tokens, output_tokens| Usage {
            input_tokens,
            cached_input_tokens,
            output_tokens,
        };
        let (first, last) = (usage(100, 50, 3), usage(10, 5, 6));
        assembler
            .push(&ResponseEvent::UsageUpdated { usage: first })
            .unwrap();
        let snapshot = assembler.snapshot();
        assembler
            .push(&ResponseEvent::UsageUpdated { usage: last })
            .unwrap();
        assert_eq!((snapshot.usage, assembler.snapshot().usage), (first, last));
        assert_eq!(assembler.snapshot().stop_reason, None);
        assembler.push(&end_with(StopReason::MaxTokens)).unwrap();
        assert!(assembler.truncated());
        assert_eq!(
            assembler.snapshot().stop_reason,
            Some(StopReason::MaxTokens)
        );
        let (items, usage, reason) = assembler.finish().unwrap();
        assert_eq!((&items[0].blocks[0].content, &items[1]), (&text(""), &tool));
        assert_eq!((usage, reason), (last, StopReason::MaxTokens));
    }

    // A literal DTO fixture pins the observation/journal snapshot shape
    // independently of the private live representation.
    #[test]
    fn rejected_transitions_preserve_live_state_and_snapshot() {
        let replay_update = ResponseEvent::ItemReplayUpdated {
            id: "i".into(),
            replay: replay(),
        };
        let open = || {
            vec![
                start("i", 0, ItemKind::Text),
                block("i", "b", 0, BlockKind::Text),
            ]
        };
        let with = |event| {
            let mut events = open();
            events.push(event);
            events
        };
        let phases = [
            vec![],
            vec![start("i", 0, ItemKind::Text)],
            open(),
            with(delta("i", "b", ContentDelta::Text("draft".into()))),
            with(end_block("i", "b", text("final"))),
            events_for_content(&[AssistantItem::text("i", 0, "final")]),
            vec![start("i", 0, ItemKind::Text), discard("i")],
            vec![end()],
        ];
        let candidates = [
            start("", 1, ItemKind::Text),
            start("i", 0, ItemKind::Text),
            start("other", 0, ItemKind::Text),
            block("missing", "b", 0, BlockKind::Text),
            block("i", "", 1, BlockKind::Text),
            block("i", "b", 1, BlockKind::Text),
            block("i", "other", 0, BlockKind::Text),
            block("i", "reason", 1, BlockKind::Reasoning),
            delta("i", "missing", ContentDelta::Text("bad".into())),
            delta("i", "b", ContentDelta::Text("next".into())),
            delta("i", "b", ContentDelta::JsonFragment("{}".into())),
            end_block("i", "b", BlockContent::Reasoning { text: "bad".into() }),
            end_block("i", "b", text("next")),
            end_item("i"),
            end_item("missing"),
            replay_update,
            discard("missing"),
            end(),
        ];
        let mut rejections = 0;
        for events in phases {
            let original = assembled(events);
            for event in &candidates {
                let mut assembler = original.clone();
                if assembler.push(event).is_err() {
                    rejections += 1;
                    assert_eq!(assembler.snapshot(), original.snapshot(), "{event:?}");
                    // Includes the discarded-ID tombstones and private live phases.
                    assert_eq!(
                        format!("{assembler:?}"),
                        format!("{original:?}"),
                        "{event:?}"
                    );
                }
            }
        }
        assert!(rejections > 100);
    }

    #[test]
    fn duplicate_tool_ids_and_tool_item_cardinality_are_rejected_atomically() {
        let mut assembler = ResponseAssembler::default();
        let call = ToolCall::new("same", "tool", json!({})).unwrap();
        let items = vec![
            AssistantItem::tool_call("first", 0, call.clone()),
            AssistantItem::tool_call("second", 1, call),
        ];
        let error = events_for_content(&items)
            .iter()
            .try_for_each(|event| assembler.push(event))
            .unwrap_err();
        assert_eq!(error.message, "duplicate tool call ID");
        let call = |id| BlockContent::ToolCall(ToolCall::new(id, "run", json!({})).unwrap());
        let mut assembler = assembled([
            start("i", 0, ItemKind::ToolCall),
            block("i", "a", 0, BlockKind::ToolCallArguments),
            block("i", "b", 1, BlockKind::ToolCallArguments),
            end_block("i", "a", call("call")),
        ]);
        let before = assembler.snapshot();
        for rejected in [end_item("i"), end_block("i", "b", call("call"))] {
            assert!(assembler.push(&rejected).is_err());
            assert_eq!(assembler.snapshot(), before);
        }
        assembler.push(&end_block("i", "b", call("other"))).unwrap();
        let before = assembler.snapshot();
        let error = assembler.push(&end_item("i")).unwrap_err();
        assert_eq!(
            error.message,
            "tool call item must have exactly one arguments block"
        );
        assert_eq!(assembler.snapshot(), before);
        assembler.push(&discard("i")).unwrap();
        assert!(assembler.snapshot().items.is_empty());
        assembler.push(&end()).unwrap();
        assert!(assembler.finish().unwrap().0.is_empty());
    }

    #[test]
    fn finish_requires_response_end_and_complete_items() {
        for events in [
            vec![],
            vec![start("i", 0, ItemKind::Reasoning)],
            events_for_content(&[AssistantItem::text("i", 0, "done")]),
        ] {
            assert!(assembled(events).finish().is_err());
        }
        assert!(assembled([end()]).finish().unwrap().0.is_empty());
    }
}

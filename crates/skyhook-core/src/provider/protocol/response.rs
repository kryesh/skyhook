use std::collections::BTreeMap;

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
                if id.trim().is_empty() || self.items.values().any(|item| item.id == *id) {
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
                                .is_some_and(|previous| previous.id == call.id)
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
                    BlockContent::ToolCall(call) => {
                        if call.id.trim().is_empty() || call.name.trim().is_empty() {
                            return Err(ProviderError::protocol(
                                "tool call ID and name must be nonempty",
                            ));
                        }
                        if !call.arguments.is_object() {
                            return Err(ProviderError::protocol(
                                "tool call arguments must be an object",
                            ));
                        }
                        call.arguments.to_string()
                    }
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
    use super::super::{Message, ToolCall};
    use super::*;
    use serde_json::json;

    fn replay() -> ReplayEnvelope {
        ReplayEnvelope {
            version: 1,
            protocol: "responses".into(),
            model: "model".into(),
            scope: "reasoning".into(),
            payload: json!({"opaque": [1, 2, 3]}),
        }
    }

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

    fn end() -> ResponseEvent {
        ResponseEvent::ResponseEnded {
            stop_reason: StopReason::EndTurn,
        }
    }

    fn text(text: &str) -> BlockContent {
        BlockContent::Text { text: text.into() }
    }

    fn assert_rejected(prefix: &[ResponseEvent], invalid: ResponseEvent) {
        let mut assembler = ResponseAssembler::default();
        for event in prefix {
            assembler.push(event).unwrap();
        }
        let before = assembler.snapshot();
        assert!(
            assembler.push(&invalid).is_err(),
            "accepted invalid event: {invalid:?}"
        );
        assert_eq!(assembler.snapshot(), before, "rejected event mutated state");
    }

    #[test]
    fn consecutive_reasoning_parts_3_3_2_and_serialization() {
        let items: Vec<_> = [3, 3, 2]
            .into_iter()
            .enumerate()
            .map(|(position, count)| AssistantItem {
                id: format!("reasoning-{position}"),
                position,
                kind: ItemKind::Reasoning,
                blocks: (0..count)
                    .map(|part| AssistantBlock {
                        id: format!("part-{part}"),
                        position: part,
                        content: BlockContent::Reasoning {
                            text: format!("{position}/{part}"),
                        },
                    })
                    .collect(),
                replay: Some(replay()),
            })
            .collect();
        let message = Message::Assistant(items.clone());
        assert_eq!(
            serde_json::from_value::<Message>(serde_json::to_value(&message).unwrap()).unwrap(),
            message
        );
        let events = events_for_content(&items);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(
                    e,
                    ResponseEvent::ItemEnded {
                        replay: Some(_),
                        ..
                    }
                ))
                .count(),
            3
        );
        assert!(!events.iter().any(|e| matches!(
            e,
            ResponseEvent::ResponseEnded { .. } | ResponseEvent::UsageUpdated { .. }
        )));
        let mut assembler = ResponseAssembler::default();
        for event in events {
            assembler.push(&event).unwrap();
        }
        assembler.push(&end()).unwrap();
        assert_eq!(
            assembler.finish().unwrap(),
            (items, Usage::default(), StopReason::EndTurn)
        );
    }

    #[test]
    fn interleaving_orders_items_and_blocks_by_position_not_arrival() {
        let mut assembler = ResponseAssembler::default();
        for event in [
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
        ] {
            assembler.push(&event).unwrap();
        }
        let (items, _, _) = assembler.finish().unwrap();
        assert_eq!(
            items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            ["earlier", "later"]
        );
        assert_eq!(
            items[1]
                .blocks
                .iter()
                .map(|b| b.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(items[1].text_content().as_deref(), Some("AB"));
    }

    #[test]
    fn empty_and_opaque_only_reasoning_are_preserved() {
        let items = vec![
            AssistantItem::reasoning("empty", 0, "", None),
            AssistantItem {
                id: "opaque".into(),
                position: 1,
                kind: ItemKind::Reasoning,
                blocks: vec![],
                replay: Some(replay()),
            },
            AssistantItem {
                id: "no-parts".into(),
                position: 2,
                kind: ItemKind::Reasoning,
                blocks: vec![],
                replay: None,
            },
        ];
        let mut assembler = ResponseAssembler::default();
        for event in events_for_content(&items) {
            assembler.push(&event).unwrap();
        }
        assembler.push(&end()).unwrap();
        assert_eq!(assembler.finish().unwrap().0, items);
    }

    #[test]
    fn final_only_and_authoritative_replacement_with_usage_snapshots() {
        let mut assembler = ResponseAssembler::default();
        assembler.push(&start("i", 0, ItemKind::Text)).unwrap();
        assembler
            .push(&block("i", "b", 0, BlockKind::Text))
            .unwrap();
        assembler
            .push(&delta("i", "b", ContentDelta::Text("draft".into())))
            .unwrap();
        let partial = assembler.snapshot();
        assert_eq!(partial.items[0].blocks[0].text, "draft");
        assert_eq!(partial.items[0].blocks[0].content, None);
        assert!(!partial.items[0].blocks[0].ended);
        assembler.push(&end_block("i", "b", text(""))).unwrap();
        assert_eq!(assembler.snapshot().items[0].blocks[0].text, "");
        assert_eq!(partial.items[0].blocks[0].text, "draft");
        assembler.push(&end_item("i")).unwrap();
        let call = ToolCall {
            id: "call".into(),
            name: "run".into(),
            arguments: json!({"a": 1}),
        };
        let tool = AssistantItem::tool_call("tool", 1, call.clone());
        for event in events_for_content(std::slice::from_ref(&tool)) {
            assembler.push(&event).unwrap();
        }
        assert_eq!(tool.tool_call_ref(), Some(&call));
        let first = Usage {
            input_tokens: 100,
            cached_input_tokens: 50,
            output_tokens: 3,
        };
        let last = Usage {
            input_tokens: 10,
            cached_input_tokens: 5,
            output_tokens: 6,
        };
        assembler
            .push(&ResponseEvent::UsageUpdated { usage: first })
            .unwrap();
        let snapshot = assembler.snapshot();
        assembler
            .push(&ResponseEvent::UsageUpdated { usage: last })
            .unwrap();
        assert_eq!(snapshot.usage, first);
        assert_eq!(assembler.snapshot().usage, last);
        assert_eq!(assembler.snapshot().stop_reason, None);
        assembler
            .push(&ResponseEvent::ResponseEnded {
                stop_reason: StopReason::MaxTokens,
            })
            .unwrap();
        assert!(assembler.truncated());
        assert_eq!(
            assembler.snapshot().stop_reason,
            Some(StopReason::MaxTokens)
        );
        let (items, usage, reason) = assembler.finish().unwrap();
        assert_eq!(items[0].blocks[0].content, text(""));
        assert_eq!(items[1], tool);
        assert_eq!(usage, last);
        assert_eq!(reason, StopReason::MaxTokens);
    }

    #[test]
    fn invalid_starts_and_order_collisions() {
        assert_rejected(&[], start("", 0, ItemKind::Text));
        let started = [start("i", 0, ItemKind::Text)];
        assert_rejected(&started, start("i", 1, ItemKind::Text));
        assert_rejected(&started, start("other", 0, ItemKind::Text));
        assert_rejected(&started, block("missing", "b", 0, BlockKind::Text));
        assert_rejected(&started, block("i", "", 0, BlockKind::Text));
        for kind in [BlockKind::Reasoning, BlockKind::ToolCallArguments] {
            assert_rejected(&started, block("i", "b", 0, kind));
        }
        let opened = [started[0].clone(), block("i", "b", 0, BlockKind::Text)];
        assert_rejected(&opened, block("i", "b", 1, BlockKind::Text));
        assert_rejected(&opened, block("i", "c", 0, BlockKind::Text));
        assert_rejected(
            &[start("r", 0, ItemKind::Reasoning)],
            block("r", "b", 0, BlockKind::Text),
        );
        assert_rejected(
            &[start("t", 0, ItemKind::ToolCall)],
            block("t", "b", 0, BlockKind::Reasoning),
        );
    }

    #[test]
    fn invalid_missing_and_closed_lifecycles() {
        let d = delta("i", "b", ContentDelta::Text("x".into()));
        let b_end = end_block("i", "b", text("x"));
        for invalid in [d.clone(), b_end.clone(), end_item("i")] {
            assert_rejected(&[], invalid);
        }
        let started = vec![start("i", 0, ItemKind::Text)];
        for invalid in [d.clone(), b_end.clone(), end_item("i"), end()] {
            assert_rejected(&started, invalid);
        }
        let mut opened = started;
        opened.push(block("i", "b", 0, BlockKind::Text));
        for invalid in [end_item("i"), end()] {
            assert_rejected(&opened, invalid);
        }
        opened.push(b_end.clone());
        for invalid in [
            d.clone(),
            b_end.clone(),
            block("i", "b", 1, BlockKind::Text),
            end(),
        ] {
            assert_rejected(&opened, invalid);
        }
        opened.push(end_item("i"));
        for invalid in [
            d,
            b_end,
            block("i", "new", 1, BlockKind::Text),
            end_item("i"),
            ResponseEvent::ItemEnded {
                id: "i".into(),
                replay: Some(replay()),
            },
            start("i", 1, ItemKind::Text),
        ] {
            assert_rejected(&opened, invalid);
        }
        opened.push(end());
        for invalid in [
            start("new", 1, ItemKind::Reasoning),
            block("i", "b", 0, BlockKind::Text),
            delta("i", "b", ContentDelta::Text("x".into())),
            end_block("i", "b", text("x")),
            end_item("i"),
            ResponseEvent::UsageUpdated {
                usage: Usage::default(),
            },
            end(),
        ] {
            assert_rejected(&opened, invalid);
        }
    }

    #[test]
    fn invalid_delta_terminal_kinds_and_tool_calls() {
        let text_prefix = [
            start("i", 0, ItemKind::Text),
            block("i", "b", 0, BlockKind::Text),
        ];
        assert_rejected(
            &text_prefix,
            delta("i", "b", ContentDelta::JsonFragment("{}".into())),
        );
        assert_rejected(
            &text_prefix,
            end_block("i", "b", BlockContent::Reasoning { text: "x".into() }),
        );
        let reason_prefix = [
            start("i", 0, ItemKind::Reasoning),
            block("i", "b", 0, BlockKind::Reasoning),
        ];
        assert_rejected(
            &reason_prefix,
            delta("i", "b", ContentDelta::JsonFragment("{}".into())),
        );
        assert_rejected(&reason_prefix, end_block("i", "b", text("x")));
        let tool_prefix = [
            start("i", 0, ItemKind::ToolCall),
            block("i", "b", 0, BlockKind::ToolCallArguments),
        ];
        assert_rejected(
            &tool_prefix,
            delta("i", "b", ContentDelta::Text("{}".into())),
        );
        assert_rejected(&tool_prefix, end_block("i", "b", text("x")));
        for arguments in [json!(null), json!([]), json!("{}"), json!(1), json!(true)] {
            assert_rejected(
                &tool_prefix,
                end_block(
                    "i",
                    "b",
                    BlockContent::ToolCall(ToolCall {
                        id: "call".into(),
                        name: "run".into(),
                        arguments,
                    }),
                ),
            );
        }
        for (id, name) in [("", "run"), ("call", ""), (" ", "run"), ("call", "\t")] {
            assert_rejected(
                &tool_prefix,
                end_block(
                    "i",
                    "b",
                    BlockContent::ToolCall(ToolCall {
                        id: id.into(),
                        name: name.into(),
                        arguments: json!({}),
                    }),
                ),
            );
        }
        let mut assembler = ResponseAssembler::default();
        for event in tool_prefix {
            assembler.push(&event).unwrap();
        }
        assembler
            .push(&delta(
                "i",
                "b",
                ContentDelta::JsonFragment("{incomplete".into()),
            ))
            .unwrap();
        let final_content = BlockContent::ToolCall(ToolCall {
            id: "call".into(),
            name: "run".into(),
            arguments: json!({}),
        });
        assembler
            .push(&end_block("i", "b", final_content.clone()))
            .unwrap();
        assert_eq!(assembler.snapshot().items[0].blocks[0].text, "{}");
        assert_eq!(
            assembler.snapshot().items[0].blocks[0].content,
            Some(final_content)
        );
    }

    #[test]
    fn tool_items_require_one_block_and_response_unique_call_ids() {
        let call = |id: &str| {
            BlockContent::ToolCall(ToolCall {
                id: id.into(),
                name: "run".into(),
                arguments: json!({}),
            })
        };
        let mut prefix = vec![
            start("i", 0, ItemKind::ToolCall),
            block("i", "a", 0, BlockKind::ToolCallArguments),
            end_block("i", "a", call("first")),
            block("i", "b", 1, BlockKind::ToolCallArguments),
        ];
        assert_rejected(&prefix, end_block("i", "b", call("first")));
        prefix.push(end_block("i", "b", call("second")));
        assert_rejected(&prefix, end_item("i"));
        let prefix = vec![
            start("i", 0, ItemKind::ToolCall),
            block("i", "a", 0, BlockKind::ToolCallArguments),
            end_block("i", "a", call("first")),
            end_item("i"),
            start("j", 1, ItemKind::ToolCall),
            block("j", "a", 0, BlockKind::ToolCallArguments),
        ];
        assert_rejected(&prefix, end_block("j", "a", call("first")));
    }

    #[test]
    fn replay_envelopes_are_opaque_and_allowed_on_any_item_kind() {
        let mut item = AssistantItem::text("text", 0, "visible");
        item.replay = Some(replay());
        let mut assembler = ResponseAssembler::default();
        for event in events_for_content(std::slice::from_ref(&item)) {
            assembler.push(&event).unwrap();
        }
        assembler.push(&end()).unwrap();
        assert_eq!(assembler.finish().unwrap().0, vec![item]);
    }

    #[test]
    fn finish_requires_response_end_and_complete_items() {
        assert!(ResponseAssembler::default().finish().is_err());
        let mut assembler = ResponseAssembler::default();
        assembler.push(&start("i", 0, ItemKind::Reasoning)).unwrap();
        assert!(assembler.finish().is_err());
        let mut assembler = ResponseAssembler::default();
        for event in events_for_content(&[AssistantItem::text("i", 0, "done")]) {
            assembler.push(&event).unwrap();
        }
        assert!(assembler.finish().is_err());
        let mut assembler = ResponseAssembler::default();
        assembler.push(&end()).unwrap();
        assert!(assembler.finish().unwrap().0.is_empty());
    }
}

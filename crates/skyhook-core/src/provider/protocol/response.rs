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
    use super::super::ToolCall;
    use super::*;
    use serde_json::json;

    #[test]
    fn discarded_partial_tools_cannot_reappear_or_execute() {
        let mut assembler = ResponseAssembler::default();
        assembler
            .push(&start("tool", 0, ItemKind::ToolCall))
            .unwrap();
        assembler
            .push(&ResponseEvent::BlockStarted {
                item: "tool".into(),
                id: "args".into(),
                position: 0,
                kind: BlockKind::ToolCallArguments,
            })
            .unwrap();
        assembler
            .push(&ResponseEvent::BlockDelta {
                item: "tool".into(),
                block: "args".into(),
                delta: ContentDelta::JsonFragment("{\"incomplete\":".into()),
            })
            .unwrap();
        assembler
            .push(&ResponseEvent::ItemDiscarded { id: "tool".into() })
            .unwrap();
        assert!(
            assembler
                .push(&start("tool", 0, ItemKind::ToolCall))
                .is_err()
        );
        assembler
            .push(&ResponseEvent::ResponseEnded {
                stop_reason: StopReason::Aborted,
            })
            .unwrap();
        let (items, _, reason) = assembler.finish().unwrap();
        assert!(items.is_empty());
        assert_eq!(reason, StopReason::Aborted);
    }

    #[test]
    fn replay_enrichment_requires_completed_item_and_preserves_content() {
        let mut assembler = ResponseAssembler::default();
        let replay = ReplayEnvelope {
            version: 1,
            protocol: "responses".into(),
            model: "m".into(),
            scope: "s".into(),
            payload: json!({"encrypted_content":"cipher"}),
        };
        assembler
            .push(&start("reason", 0, ItemKind::Reasoning))
            .unwrap();
        let update = ResponseEvent::ItemReplayUpdated {
            id: "reason".into(),
            replay: replay.clone(),
        };
        assert!(assembler.push(&update).is_err());
        assembler
            .push(&ResponseEvent::ItemEnded {
                id: "reason".into(),
                replay: None,
            })
            .unwrap();
        assembler.push(&update).unwrap();
        assembler
            .push(&ResponseEvent::ResponseEnded {
                stop_reason: StopReason::EndTurn,
            })
            .unwrap();
        assert!(assembler.push(&update).is_err());
        let (items, _, _) = assembler.finish().unwrap();
        assert_eq!(items[0].replay, Some(replay));
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

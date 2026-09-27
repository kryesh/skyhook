//! Dispatch a delta's output by block kind, assemble visible text, and build
//! the completed items.
use super::super::{ReasoningFormat, wire};
use super::{Block, Decoder};
use crate::provider::{
    ProviderError,
    codec::common::{self, replay},
    protocol::{AssistantItem, Binding, ItemKind, ReplayFormat, ResponseEvent},
};
use serde_json::json;

impl Decoder {
    pub(super) fn delta(&mut self, delta: &wire::Delta, events: &mut Vec<ResponseEvent>) {
        match (
            self.format,
            &delta.thinking_blocks,
            &delta.reasoning_details,
        ) {
            (ReasoningFormat::ThinkingBlocks, Some(blocks), _) if !blocks.is_empty() => {
                for block in blocks {
                    self.thinking_block(block, events);
                }
            }
            (ReasoningFormat::Details, _, Some(details)) if !details.is_empty() => {
                for detail in details {
                    self.detail(detail, events);
                }
            }
            _ => {
                if let Some(text) = delta.reasoning_text() {
                    self.visible_delta(&text, true, events);
                }
            }
        }
        for text in [delta.content.as_deref(), delta.refusal.as_deref()]
            .into_iter()
            .flatten()
        {
            if !text.is_empty() {
                self.visible_delta(text, false, events);
            }
        }
        self.tool_deltas(delta, events);
    }

    /// The part of a full message not already streamed. A message that
    /// repeats or extends the stream adds only what is new; one that differs
    /// (trimmed, reformatted) leaves the streamed content as it is.
    pub(super) fn unstreamed(&self, mut message: wire::Delta) -> wire::Delta {
        let streamed = |reasoning: bool| -> String {
            self.blocks
                .iter()
                .filter_map(|block| match (block, reasoning) {
                    (Block::Text(text), false) | (Block::Reasoning(text), true) => {
                        Some(text.as_str())
                    }
                    (Block::Native { object, shape }, true) => Some(shape.text(object)),
                    _ => None,
                })
                .collect()
        };
        let remainder = |streamed: String, full: Option<&str>| {
            full.and_then(|full| full.strip_prefix(streamed.as_str()))
                .filter(|rest| !rest.is_empty())
                .map(str::to_owned)
        };
        let visible = [message.content.as_deref(), message.refusal.as_deref()]
            .into_iter()
            .flatten()
            .collect::<String>();
        let streamed_native = self
            .blocks
            .iter()
            .any(|block| matches!(block, Block::Native { .. }));
        // Whole native objects come from the message only in the field this decoder
        // replays, and only when none were streamed; any other field would repeat
        // reasoning as text.
        let native = |format| self.format == format && !streamed_native;
        let mut missing = wire::Delta {
            reasoning_content: remainder(streamed(true), message.reasoning_text().as_deref()),
            content: remainder(streamed(false), Some(&visible)),
            thinking_blocks: message
                .thinking_blocks
                .take()
                .filter(|_| native(ReasoningFormat::ThinkingBlocks)),
            reasoning_details: message
                .reasoning_details
                .take()
                .filter(|_| native(ReasoningFormat::Details)),
            ..Default::default()
        };
        // Tool calls come from the message only when none were streamed.
        let streamed_tools = self
            .blocks
            .iter()
            .any(|block| matches!(block, Block::Tool { .. }));
        if !streamed_tools {
            let legacy = message.legacy_call();
            missing.tool_calls = Some(
                message
                    .tool_calls
                    .into_iter()
                    .flatten()
                    .chain(legacy)
                    .enumerate()
                    .map(|(index, call)| wire::ToolDelta {
                        index: Some(index as u64),
                        ..call
                    })
                    .collect(),
            );
        }
        missing
    }

    /// Continue the text or plain reasoning block receiving output, or open one.
    fn visible_delta(&mut self, text: &str, reasoning: bool, events: &mut Vec<ResponseEvent>) {
        let same = self.visible_id.filter(|id| {
            matches!(
                (&self.blocks[*id], reasoning),
                (Block::Reasoning(_), true) | (Block::Text(_), false)
            )
        });
        let id = if let Some(id) = same {
            id
        } else {
            let id = self.blocks.len();
            self.blocks.push(if reasoning {
                Block::Reasoning(String::new())
            } else {
                Block::Text(String::new())
            });
            self.visible_id = Some(id);
            id
        };
        match &mut self.blocks[id] {
            Block::Text(full) | Block::Reasoning(full) => full.push_str(text),
            _ => unreachable!(),
        }
        let kind = if reasoning {
            ItemKind::Reasoning
        } else {
            ItemKind::Text
        };
        events.push(common::delta(id, kind, text));
    }

    /// The completed items in block order. Tool calls are dropped when the finish
    /// does not authorize them, and otherwise must be complete and well formed.
    pub(super) fn items(&self, discard_tools: bool) -> Result<Vec<AssistantItem>, ProviderError> {
        let mut ids = self.call_ids();
        let details_signed = self.details_signed();
        let mut items = Vec::new();
        for (index, block) in self.blocks.iter().enumerate() {
            let (id, position) = (index.to_string(), common::position(index)?);
            items.push(match block {
                Block::Text(text) => AssistantItem::text(id, position, text.clone()),
                Block::Reasoning(text) => {
                    let payload = json!({"text": text});
                    let replay = replay(
                        ReplayFormat::ChatText,
                        &self.model,
                        &self.scope,
                        payload,
                        Binding::Free,
                    );
                    AssistantItem::reasoning(id, position, text.clone(), Some(replay))
                }
                Block::Native { object, shape } => {
                    let replay = self.native_replay(object, *shape, details_signed);
                    let text = shape.text(object).to_owned();
                    AssistantItem::reasoning(id, position, text, Some(replay))
                }
                Block::Tool { .. } if discard_tools => continue,
                Block::Tool {
                    call_id,
                    name,
                    arguments,
                } => {
                    let call = self.tool_call(index, &mut ids, call_id, name, arguments)?;
                    AssistantItem::tool_call(id, position, call)
                }
            });
        }
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use crate::provider::protocol::{ItemKind, Outcome};
    use serde_json::json;

    #[test]
    fn content_refusal_reasoning_and_tools_decode_as_ordered_items() {
        for (frames, expected_outcome, expected) in [
            // A singleton choice without an index is read like choice zero.
            (
                vec![
                    event(json!({"choices":[{"delta":{"reasoning_content":"loading"}}]})),
                    end("stop"),
                ],
                Outcome::Answer,
                vec![reasoning("loading")],
            ),
            // Refusal is visible completed output.
            (
                vec![
                    delta(json!({"refusal":"I cannot "})),
                    delta(json!({"refusal":"help with that."})),
                    end("stop"),
                ],
                Outcome::Answer,
                vec![text("I cannot help with that.")],
            ),
            // Equal reasoning aliases are not duplicated, and null fields are no-ops.
            (
                vec![
                    delta(json!({"reasoning":"same","reasoning_content":"same"})),
                    delta(
                        json!({"role":null,"content":null,"reasoning":null,"function_call":null}),
                    ),
                    end("stop"),
                ],
                Outcome::Answer,
                vec![reasoning("same")],
            ),
            // One delta repeats an index for a header and its sequential arguments.
            (
                vec![
                    tool_delta(json!([
                        {"index":0,"id":"call-a","type":"function","function":{"name":"inspect","arguments":""}},
                        {"index":0,"function":{"arguments":"{\"path\":"}},
                        {"index":1,"id":"call-b","function":{"name":"other","arguments":"{}"}},
                        {"index":0,"function":{"arguments":"\"file\"}"}}
                    ])),
                    end("tool_calls"),
                ],
                Outcome::ToolUse,
                vec![
                    tool("call-a", "inspect", json!({"path":"file"})),
                    tool("call-b", "other", json!({})),
                ],
            ),
            // Alternating reasoning and text remain separate and ordered.
            (
                vec![
                    delta(
                        json!({"role":"assistant", "content":null, "reasoning_content":"first thought"}),
                    ),
                    delta(json!({"content":"hello "})),
                    delta(json!({"reasoning":"second thought"})),
                    delta(json!({"content":"世界"})),
                    end("stop"),
                ],
                Outcome::Answer,
                vec![
                    reasoning("first thought"),
                    text("hello "),
                    reasoning("second thought"),
                    text("世界"),
                ],
            ),
        ] {
            let (items, _, outcome) = decode(frames);
            assert_eq!(outcome, expected_outcome);
            assert_eq!(contents(&items), expected);
            for (position, item) in items.iter().enumerate() {
                assert_eq!(item.position().get(), u32::try_from(position).unwrap());
                assert_eq!(item.replay().is_some(), item.kind() == ItemKind::Reasoning);
            }
        }
    }

    #[test]
    fn unknown_and_malformed_delta_fields_are_ignored() {
        // Unknown fields, and known ones with unusable shapes.
        for field in [
            "unknown",
            "audio",
            "provider_specific_fields",
            "thinking_blocks",
            "reasoning_details",
        ] {
            for value in [
                json!(""),
                json!(false),
                json!(0),
                json!([]),
                json!({"data":"x"}),
            ] {
                let mut payload = json!({"content":"answer"});
                payload[field] = value;
                let (items, _, _) = decode(vec![delta(payload), end("stop")]);
                assert_eq!(contents(&items), [text("answer")]);
            }
        }
    }

    #[test]
    fn conflicting_reasoning_aliases_prefer_reasoning_content() {
        let (items, _, _) = decode(vec![
            delta(json!({"reasoning":"alias","reasoning_content":"primary"})),
            delta(json!({"reasoning":{}, "content":[{"type":"text","text":"answer"}]})),
            end("stop"),
        ]);
        assert_eq!(contents(&items), [reasoning("primary"), text("answer")]);
    }
}

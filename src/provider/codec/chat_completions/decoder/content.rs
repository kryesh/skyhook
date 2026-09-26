//! Assemble visible text, reasoning envelopes, and sequential tool fragments.
use super::super::wire;
use super::{Block, Decoder, NativeShape};
use crate::provider::{
    ProviderError,
    codec::common::{self, parse_tool_arguments, replay},
    protocol::{AssistantItem, Binding, ItemKind, ReplayFormat, ResponseEvent, ToolCall},
};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

impl Decoder {
    pub(super) fn delta(
        &mut self,
        delta: &wire::Delta,
        events: &mut Vec<ResponseEvent>,
    ) -> Result<(), ProviderError> {
        if delta.is_noop(self.format) {
            return Ok(());
        }
        match (
            self.format,
            &delta.thinking_blocks,
            &delta.reasoning_details,
        ) {
            (ReplayFormat::ChatThinkingBlock, Some(blocks), _) if !blocks.is_empty() => {
                for block in blocks {
                    self.thinking_block(block, events);
                }
            }
            (ReplayFormat::ChatReasoningDetail, _, Some(details)) if !details.is_empty() => {
                for detail in details {
                    self.detail(detail, events);
                }
            }
            _ => {
                if let Some(text) = delta.reasoning_text() {
                    self.visible_delta(&text, true, events)?;
                }
            }
        }
        for text in [delta.content.as_deref(), delta.refusal.as_deref()]
            .into_iter()
            .flatten()
        {
            if !text.is_empty() {
                self.visible_delta(text, false, events)?;
            }
        }
        let legacy = delta.legacy_call();
        // Unindexed entries seen in this delta; another named entry is a new call.
        let mut touched = BTreeSet::new();
        for call in delta.tool_calls.iter().flatten().chain(legacy.as_ref()) {
            let index = self.tool_index(call, &touched);
            if call.index.is_none() {
                touched.insert(index);
            }
            // A Hermes delta may contain a header and argument fragments for
            // the same index. Apply each entry in wire order, not as a map.
            let id = if let Some(id) = self.tool_ids.get(&index) {
                *id
            } else {
                let id = self.blocks.len();
                self.blocks.push(Block::Tool {
                    call_id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                });
                self.tool_ids.insert(index, id);
                id
            };
            self.last_tool = Some(index);
            let Block::Tool {
                call_id,
                name,
                arguments,
            } = &mut self.blocks[id]
            else {
                unreachable!()
            };
            let function = call.function.as_ref();
            let id_fragment = call.id.as_deref();
            let name_fragment = function.and_then(|function| function.name.as_deref());
            // Some servers repeat the whole header on every chunk: a fragment equal
            // to the value so far is a repeat once it comes with or after arguments.
            let carries_arguments = function.is_some_and(|function| function.arguments.is_some());
            let header_repeated = !arguments.is_empty()
                || carries_arguments
                || id_fragment.is_some_and(|fragment| !fragment.is_empty() && fragment == call_id);
            for (current, fragment) in [(call_id, id_fragment), (name, name_fragment)] {
                if let Some(fragment) = fragment
                    && !(header_repeated && fragment == current.as_str())
                {
                    current.push_str(fragment);
                }
            }
            if let Some(fragment) = function.and_then(|function| function.arguments.as_ref()) {
                arguments.push_str(fragment);
                if !fragment.is_empty() {
                    events.push(common::delta(
                        common::block_ref(id),
                        ItemKind::ToolCall,
                        fragment.clone(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// The part of a full message not already streamed. A message that
    /// repeats or extends the stream adds only what is new; one that differs
    /// (trimmed, reformatted) leaves the streamed content as it is.
    pub(super) fn unstreamed(&self, mut message: wire::Delta) -> wire::Delta {
        let streamed = |reasoning: bool| -> String {
            self.blocks
                .iter()
                .filter_map(|block| match (block, reasoning) {
                    (Block::Text(text), false)
                    | (Block::Reasoning(text), true)
                    | (Block::Native { text, .. }, true) => Some(text.as_str()),
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
                .filter(|_| native(ReplayFormat::ChatThinkingBlock)),
            reasoning_details: message
                .reasoning_details
                .take()
                .filter(|_| native(ReplayFormat::ChatReasoningDetail)),
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

    /// The wire index of a tool fragment. Unindexed fragments continue the latest
    /// call unless their ID, name, or a new object shows a different call.
    fn tool_index(&self, call: &wire::ToolDelta, touched: &BTreeSet<u64>) -> u64 {
        if let Some(index) = call.index {
            return index;
        }
        // Server indices are arbitrary; never overflow.
        let next = || {
            self.tool_ids
                .keys()
                .max()
                .map_or(Some(0), |index| index.checked_add(1))
                .unwrap_or_else(|| {
                    (0..)
                        .find(|index| !self.tool_ids.contains_key(index))
                        .expect("fewer calls than indices")
                })
        };
        let Some(index) = self.last_tool else {
            return next();
        };
        let Block::Tool {
            call_id,
            name,
            arguments,
        } = &self.blocks[self.tool_ids[&index]]
        else {
            unreachable!()
        };
        let function = call.function.as_ref();
        let id = call.id.as_deref().filter(|id| !id.is_empty());
        let new_name = function
            .and_then(|function| function.name.as_deref())
            .filter(|name| !name.is_empty());
        let fragment = function.and_then(|function| function.arguments.as_deref());
        let id_differs = id.is_some_and(|id| !call_id.is_empty() && id != call_id);
        let name_differs = new_name.is_some_and(|new_name| !name.is_empty() && new_name != name);
        let id_after_anonymous =
            id.is_some() && call_id.is_empty() && new_name.is_some() && !name.is_empty();
        let named_again_in_delta = new_name.is_some() && touched.contains(&index);
        let new_object_after_complete = new_name.is_some()
            && !arguments.trim().is_empty()
            && parse_tool_arguments(arguments).is_some()
            && fragment.is_some_and(|fragment| fragment.trim_start().starts_with('{'));
        let new_call = id_differs
            || name_differs
            || id_after_anonymous
            || named_again_in_delta
            || new_object_after_complete;
        if new_call { next() } else { index }
    }

    fn visible_delta(
        &mut self,
        text: &str,
        reasoning: bool,
        events: &mut Vec<ResponseEvent>,
    ) -> Result<(), ProviderError> {
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
        events.push(common::delta(common::block_ref(id), kind, text));
        Ok(())
    }

    /// A `thinking_blocks` entry. Thinking fragments continue the block
    /// receiving output until an entry completes it; redacted thinking stands
    /// alone. A completing entry re-sends either the block's text so far (a
    /// snapshot, of which only the unstreamed part is new) or an empty marker;
    /// any other text it carries is a fragment.
    fn thinking_block(
        &mut self,
        block: &wire::Native<wire::ThinkingBlock>,
        events: &mut Vec<ResponseEvent>,
    ) {
        let fragment = block.view.text().unwrap_or("");
        let open = match block.view {
            wire::ThinkingBlock::Other => return,
            wire::ThinkingBlock::Thinking { .. } => self.visible_id.filter(|id| {
                matches!(
                    self.blocks[*id],
                    Block::Native {
                        shape: NativeShape::Thinking {
                            complete: false,
                            ..
                        },
                        ..
                    }
                )
            }),
            wire::ThinkingBlock::Redacted { .. } => None,
        };
        if open.is_none() && fragment.is_empty() && !block.view.completes() {
            return;
        }
        let id = open.unwrap_or_else(|| {
            self.open_native(NativeShape::Thinking {
                complete: false,
                signed: false,
            })
        });
        let Block::Native { text, shape, .. } = &mut self.blocks[id] else {
            unreachable!()
        };
        let new = if block.view.completes() {
            fragment.strip_prefix(text.as_str()).unwrap_or(fragment)
        } else {
            fragment
        };
        *shape = NativeShape::Thinking {
            complete: block.view.completes(),
            signed: block.view.is_signed(),
        };
        self.native_fragment(id, "thinking", new, &block.raw, events);
    }

    /// A `reasoning_details` entry, continuing the detail at its index. Every
    /// entry is replayed, even an empty one.
    fn detail(&mut self, detail: &wire::Native<wire::Detail>, events: &mut Vec<ResponseEvent>) {
        let view = &detail.view;
        let open = view.index.and_then(|index| {
            self.blocks.iter().rposition(|block| {
                matches!(block, Block::Native {
                    shape: NativeShape::Detail { index: Some(open), .. },
                    ..
                } if *open == index)
            })
        });
        let id = open.unwrap_or_else(|| {
            self.open_native(NativeShape::Detail {
                index: view.index,
                kind: view.kind,
                signed: false,
            })
        });
        let Block::Native {
            shape: NativeShape::Detail { kind, signed, .. },
            ..
        } = &mut self.blocks[id]
        else {
            unreachable!()
        };
        *signed |= view.signed;
        let field = kind.text_field();
        self.native_fragment(
            id,
            field,
            view.text.as_deref().unwrap_or(""),
            &detail.raw,
            events,
        );
    }

    fn open_native(&mut self, shape: NativeShape) -> usize {
        let id = self.blocks.len();
        self.blocks.push(Block::Native {
            text: String::new(),
            object: Map::new(),
            shape,
        });
        self.visible_id = Some(id);
        id
    }

    /// Append `new` to a native object's text, and merge the fragment's other
    /// fields (later values win). A text field the fragment spells, even
    /// empty, holds the whole text so far.
    fn native_fragment(
        &mut self,
        id: usize,
        field: &str,
        new: &str,
        raw: &Map<String, Value>,
        events: &mut Vec<ResponseEvent>,
    ) {
        let Block::Native { text, object, .. } = &mut self.blocks[id] else {
            unreachable!()
        };
        text.push_str(new);
        for (name, value) in raw {
            let value = if name == field && value.is_string() {
                json!(text)
            } else {
                value.clone()
            };
            object.insert(name.clone(), value);
        }
        if !new.is_empty() {
            events.push(common::delta(
                common::block_ref(id),
                ItemKind::Reasoning,
                new,
            ));
        }
    }

    /// The completed items in block order. Tool calls are dropped when the finish
    /// does not authorize them, and otherwise must be complete and well formed.
    pub(super) fn items(&self, discard_tools: bool) -> Result<Vec<AssistantItem>, ProviderError> {
        // Server-supplied IDs are reserved so derived ones never collide with them.
        let origin = self.call_id_origin();
        let reserved: BTreeSet<&str> = self
            .blocks
            .iter()
            .filter_map(|block| match block {
                Block::Tool { call_id, .. } if !call_id.trim().is_empty() => Some(call_id.as_str()),
                _ => None,
            })
            .collect();
        // Details replay as one sequence, so one signed detail binds them all.
        let details_signed = self.blocks.iter().any(|block| {
            matches!(
                block,
                Block::Native {
                    shape: NativeShape::Detail { signed: true, .. },
                    ..
                }
            )
        });
        let mut call_ids = BTreeSet::new();
        let mut items = Vec::new();
        for (id, block) in self.blocks.iter().enumerate() {
            let position = common::position(id)?;
            let item = match block {
                Block::Text(text) => AssistantItem::Text {
                    id: common::item_id(id),
                    position,
                    blocks: common::single_block(id, text.clone()),
                },
                Block::Reasoning(text) => AssistantItem::Reasoning {
                    id: common::item_id(id),
                    position,
                    blocks: common::single_block(id, text.clone()),
                    replay: Some(replay(
                        ReplayFormat::ChatText,
                        &self.model,
                        &self.scope,
                        json!({"text": text}),
                        Binding::Free,
                    )),
                },
                // A signed object is bound to the exact conversation before it,
                // like Messages thinking; an unsigned one replays freely.
                Block::Native {
                    text,
                    object,
                    shape,
                } => {
                    let (format, signed) = match *shape {
                        NativeShape::Thinking { signed, .. } => {
                            (ReplayFormat::ChatThinkingBlock, signed)
                        }
                        NativeShape::Detail { .. } => {
                            (ReplayFormat::ChatReasoningDetail, details_signed)
                        }
                    };
                    let binding = if signed {
                        Binding::Conversation
                    } else {
                        Binding::Free
                    };
                    AssistantItem::Reasoning {
                        id: common::item_id(id),
                        position,
                        blocks: common::single_block(id, text.clone()),
                        replay: Some(replay(
                            format,
                            &self.model,
                            &self.scope,
                            Value::Object(object.clone()),
                            binding,
                        )),
                    }
                }
                Block::Tool {
                    call_id,
                    name,
                    arguments,
                } => {
                    if discard_tools {
                        continue;
                    }
                    if name.trim().is_empty() {
                        return Err(ProviderError::protocol(
                            "Chat tool call has no function name",
                        ));
                    }
                    let mut call_id = call_id.clone();
                    if call_id.trim().is_empty() || call_ids.contains(&call_id) {
                        call_id = std::iter::once(format!("call_{origin}_{id}"))
                            .chain((1..).map(|n| format!("call_{origin}_{id}_{n}")))
                            .find(|candidate| {
                                !reserved.contains(candidate.as_str())
                                    && !call_ids.contains(candidate)
                            })
                            .expect("an unused ID exists");
                    }
                    call_ids.insert(call_id.clone());
                    let arguments = parse_tool_arguments(arguments).ok_or_else(|| {
                        ProviderError::protocol("Invalid Chat tool arguments JSON")
                    })?;
                    let call = ToolCall::new(call_id, name.clone(), Value::Object(arguments))
                        .map_err(|error| ProviderError::protocol(format!("Chat: {error}")))?;
                    AssistantItem::ToolCall {
                        id: common::item_id(id),
                        position,
                        call,
                    }
                }
            };
            items.push(item);
        }
        Ok(items)
    }

    /// Deterministic origin for missing IDs: a digest of the request and the
    /// response ID (or content), so IDs differ across turns and stay hex.
    fn call_id_origin(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(self.request_digest.as_bytes());
        hasher.update(self.model.as_bytes());
        match &self.response_id {
            Some(id) => {
                hasher.update(b"i");
                hasher.update(id.as_bytes());
            }
            None => {
                for block in &self.blocks {
                    let (tag, parts): (&[u8], [&str; 3]) = match block {
                        Block::Text(text) => (b"t", [text, "", ""]),
                        Block::Reasoning(text) | Block::Native { text, .. } => {
                            (b"r", [text, "", ""])
                        }
                        Block::Tool {
                            call_id,
                            name,
                            arguments,
                        } => (b"c", [call_id, name, arguments]),
                    };
                    hasher.update(tag);
                    for part in parts {
                        hasher.update((part.len() as u64).to_le_bytes());
                        hasher.update(part.as_bytes());
                    }
                }
            }
        }
        let digest = crate::media::BlobDigest::from_bytes(hasher.finalize().into());
        digest.to_string()[..16].to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use crate::provider::codec::common::tests::{Reduced, reduce};
    use crate::provider::http::transport::SseEvent;
    use crate::provider::protocol::{
        AssistantItem, Binding, ItemKind, Outcome, ReplayFormat, ResponseEvent, ToolCall,
    };
    use serde_json::{Value, json};
    use std::collections::BTreeSet;

    fn reasoning(text: &str) -> Content {
        Content::Reasoning(text.into())
    }

    fn tool(id: &str, name: &str, arguments: Value) -> Content {
        Content::Tool(ToolCall::new(id, name, arguments).unwrap())
    }

    const SYNTHETIC: &str = "<synthetic>";

    fn normalize(call: &ToolCall) -> (String, String, Value) {
        let id = if call.id().starts_with("call_") && call.id().matches('_').count() >= 2 {
            SYNTHETIC.to_owned()
        } else {
            call.id().to_owned()
        };
        (
            id,
            call.name().to_owned(),
            Value::Object(call.arguments().clone()),
        )
    }

    fn calls_of(items: &[AssistantItem]) -> Vec<(String, String, Value)> {
        let calls: Vec<_> = items.iter().filter_map(AssistantItem::call).collect();
        let ids: BTreeSet<_> = calls.iter().map(|call| call.id()).collect();
        assert_eq!(ids.len(), calls.len(), "tool call IDs must be unique");
        calls.into_iter().map(normalize).collect()
    }

    fn tool_delta(calls: Value) -> SseEvent {
        delta(json!({ "tool_calls": calls }))
    }

    fn call(id: &str, arguments: &str) -> Value {
        json!([{"index":0,"id":id,"function":{"name":"inspect","arguments":arguments}}])
    }

    /// Asserts frames before `failing` decode, and that frame and finish are rejected.
    fn assert_calls(mut frames: Vec<SseEvent>, expected: Vec<(&str, &str, Value)>) {
        frames.push(end("tool_calls"));
        let (items, _, outcome) = decode(frames);
        assert_eq!(outcome, Outcome::ToolUse);
        let expected: Vec<_> = expected
            .into_iter()
            .map(|(id, name, arguments)| (id.to_owned(), name.to_owned(), arguments))
            .collect();
        assert_eq!(calls_of(&items), expected);
    }

    fn assert_fails_at(frames: Vec<SseEvent>, failing: usize) {
        let mut decoder = decoder();
        for frame in &frames[..failing] {
            decoder.decode(frame).unwrap();
        }
        let frame = &frames[failing];
        assert!(decoder.decode(frame).is_err(), "{}", frame.data);
        assert!(decoder.finish().is_err(), "{}", frame.data);
    }

    #[test]
    fn content_refusal_reasoning_and_tools_decode_as_ordered_items() {
        for (frames, expected_outcome, expected) in [
            // A singleton choice without an index (llama loading) is reasoning.
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
            // Hermes repeats an index for a header and its sequential arguments.
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
    fn thinking_blocks_and_vendor_fields_decode() {
        let chunk = |delta: Value| {
            event(json!({"id":"chatcmpl-1","created":1,"model":"model-a",
                "object":"chat.completion.chunk","choices":[{"index":0,"delta":delta}],
                "provider_specific_fields":{}}))
        };
        let frames = vec![
            chunk(
                json!({"reasoning_content":"","thinking_blocks":[{"type":"thinking","thinking":""}],
                "provider_specific_fields":{"reasoningContent":{"text":""}},"content":"","role":"assistant"}),
            ),
            chunk(
                json!({"reasoning_content":"Compare.","thinking_blocks":[{"type":"thinking","thinking":"Compare."}],
                "provider_specific_fields":{"reasoningContent":{"text":"Compare."}},"content":""}),
            ),
            chunk(
                json!({"reasoning_content":"","thinking_blocks":[{"type":"thinking","signature":"sig","thinking":""}],
                "provider_specific_fields":{"reasoningContent":{"signature":"sig"}},"content":""}),
            ),
            chunk(json!({"content":"399 is larger."})),
            chunk(
                json!({"content":"","tool_calls":[{"id":"tooluse_1","function":{"arguments":"","name":"run"},"type":"function","index":0}]}),
            ),
            chunk(
                json!({"content":"","tool_calls":[{"function":{"arguments":""},"type":"function","index":0}]}),
            ),
            chunk(
                json!({"content":"","tool_calls":[{"function":{"arguments":"{\"cmd\": \"ls\"}"},"type":"function","index":0}]}),
            ),
            chunk(
                json!({"content":"","tool_calls":[{"id":"tooluse_2","function":{"arguments":"","name":"list_jobs"},"type":"function","index":1}]}),
            ),
            chunk(
                json!({"content":"","tool_calls":[{"function":{"arguments":"{}"},"type":"function","index":1}]}),
            ),
            event(
                json!({"choices":[{"finish_reason":"tool_calls","index":0,"delta":{}}],"provider_specific_fields":{}}),
            ),
            event(
                json!({"choices":[{"index":0,"delta":{}}],"usage":{"completion_tokens":84,"prompt_tokens":447,
                "total_tokens":531,"completion_tokens_details":{"reasoning_tokens":0,"text_tokens":84},
                "prompt_tokens_details":{"cached_tokens":0,"text_tokens":447,"cache_creation_tokens":0},
                "cache_creation_input_tokens":0,"cache_read_input_tokens":0}}),
            ),
            done(),
        ];
        let (items, usage, outcome) = decode(frames.clone());
        assert_eq!(outcome, Outcome::ToolUse);
        assert_eq!((usage.input_tokens, usage.output_tokens), (447, 84));
        let expected = [
            reasoning("Compare."),
            text("399 is larger."),
            tool("tooluse_1", "run", json!({"cmd":"ls"})),
            tool("tooluse_2", "list_jobs", json!({})),
        ];
        assert_eq!(contents(&items), expected);
        let text_replay = items[0].replay().unwrap();
        assert_eq!(text_replay.provenance.format, ReplayFormat::ChatText);
        assert_eq!(text_replay.payload, json!({"text":"Compare."}));
        // A dialect replaying thinking blocks keeps each signed block whole and
        // binds it to the conversation. LiteLLM's Bedrock path completes a block
        // with an empty marker, which keeps the text streamed before it. An
        // unknown or malformed entry leaves the open block as it is.
        let frames: Vec<_> = frames
            .into_iter()
            .take(3)
            .chain([
                chunk(json!({"thinking_blocks":[{"type":"thinking","thinking":"Again"}]})),
                chunk(json!({"thinking_blocks":[{"type":"future_block","thinking":"x"},
                    {"type":"redacted_thinking","thinking":"y"},{"type":"thinking","thinking":"!"}]})),
                chunk(
                    json!({"thinking_blocks":[{"type":"thinking","thinking":"","signature":"sig2"}]}),
                ),
                chunk(json!({"thinking_blocks":[{"type":"redacted_thinking","data":"opaque"}]})),
                chunk(json!({"content":"399 is larger."})),
                end("stop"),
            ])
            .collect();
        let reduced = decode_with(ReplayFormat::ChatThinkingBlock, frames);
        assert_eq!(
            contents(reduced.items()),
            [
                reasoning("Compare."),
                reasoning("Again!"),
                reasoning(""),
                text("399 is larger."),
            ]
        );
        assert_eq!(
            native_replays(&reduced, ReplayFormat::ChatThinkingBlock),
            [
                (
                    json!({"type":"thinking","thinking":"Compare.","signature":"sig"}),
                    Binding::Conversation
                ),
                (
                    json!({"type":"thinking","thinking":"Again!","signature":"sig2"}),
                    Binding::Conversation
                ),
                (
                    json!({"type":"redacted_thinking","data":"opaque"}),
                    Binding::Conversation
                ),
            ]
        );
    }

    fn decode_with(format: ReplayFormat, frames: Vec<SseEvent>) -> Reduced {
        let mut decoder = decoder_for(format);
        let mut events = Vec::new();
        for frame in &frames {
            events.extend(decoder.decode(frame).unwrap());
        }
        events.extend(decoder.finish().unwrap());
        reduce(events)
    }

    /// The payload and binding of each reasoning item, all of `format`.
    fn native_replays(reduced: &Reduced, format: ReplayFormat) -> Vec<(Value, Binding)> {
        reduced
            .items()
            .iter()
            .filter_map(AssistantItem::replay)
            .map(|replay| {
                assert_eq!(replay.provenance.format, format);
                (replay.payload.clone(), replay.binding)
            })
            .collect()
    }

    #[test]
    fn litellm_anthropic_signature_chunks_resend_the_block_text() {
        // LiteLLM's Anthropic path: each thinking delta carries its fragment,
        // and the signature delta re-sends the block's text so far.
        let thinking = |text: &str| {
            delta(
                json!({"reasoning_content":text,"thinking_blocks":[{"type":"thinking","thinking":text}]}),
            )
        };
        let signed = |text: &str, signature: &str| {
            delta(json!({"reasoning_content":"",
                "thinking_blocks":[{"type":"thinking","thinking":text,"signature":signature}]}))
        };
        let frames = vec![
            // A block without thinking deltas: its signature arrives with empty text.
            signed("", "sig1"),
            thinking("Weigh "),
            thinking("both."),
            signed("Weigh both.", "sig2"),
            delta(json!({"thinking_blocks":[{"type":"redacted_thinking","data":"opaque"}]})),
            delta(json!({"content":"399."})),
            end("stop"),
        ];
        let reduced = decode_with(ReplayFormat::ChatThinkingBlock, frames.clone());
        assert_eq!(reduced.streamed(ItemKind::Reasoning), ["Weigh both."]);
        assert_eq!(
            contents(reduced.items()),
            [
                reasoning(""),
                reasoning("Weigh both."),
                reasoning(""),
                text("399."),
            ]
        );
        let block = |text: &str, signature: &str| json!({"type":"thinking","thinking":text,"signature":signature});
        let bound = Binding::Conversation;
        assert_eq!(
            native_replays(&reduced, ReplayFormat::ChatThinkingBlock),
            [
                (block("", "sig1"), bound),
                (block("Weigh both.", "sig2"), bound),
                (json!({"type":"redacted_thinking","data":"opaque"}), bound),
            ]
        );
        // Where the blocks are not replayed, the snapshot adds no text either.
        let (items, _, _) = self::decode(frames);
        assert_eq!(contents(&items), [reasoning("Weigh both."), text("399.")]);
    }

    #[test]
    fn native_objects_repeated_by_the_final_message_are_kept_once() {
        let redacted = json!({"type":"redacted_thinking","data":"opaque"});
        let reduced = decode_with(
            ReplayFormat::ChatThinkingBlock,
            vec![
                delta(json!({ "thinking_blocks": [redacted] })),
                event(
                    json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop",
                    "message":{"content":"answer","thinking_blocks":[redacted]}}]}),
                ),
            ],
        );
        assert_eq!(contents(reduced.items()), [reasoning(""), text("answer")]);
        // Without a streamed object, the message supplies it.
        let reduced = decode_with(
            ReplayFormat::ChatThinkingBlock,
            vec![event(
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop",
                "message":{"content":"answer","thinking_blocks":[redacted]}}]}),
            )],
        );
        assert_eq!(
            native_replays(&reduced, ReplayFormat::ChatThinkingBlock),
            [(redacted, Binding::Conversation)]
        );
    }

    #[test]
    fn empty_native_arrays_keep_the_reasoning_text() {
        for replay in [
            ReplayFormat::ChatThinkingBlock,
            ReplayFormat::ChatReasoningDetail,
        ] {
            let reduced = decode_with(
                replay,
                vec![
                    delta(
                        json!({"reasoning_content":"Weigh.","thinking_blocks":[],"reasoning_details":[]}),
                    ),
                    delta(json!({"content":"answer"})),
                    end("stop"),
                ],
            );
            assert_eq!(
                contents(reduced.items()),
                [reasoning("Weigh."), text("answer")]
            );
        }
    }

    #[test]
    fn final_message_native_fields_outside_the_replayed_one_add_no_text() {
        for replay in [ReplayFormat::ChatText, ReplayFormat::ChatReasoningDetail] {
            let reduced = decode_with(
                replay,
                vec![
                    delta(json!({"reasoning_content":"Weigh."})),
                    event(
                        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop",
                        "message":{"content":"answer","reasoning_content":"Weigh.",
                        "thinking_blocks":[{"type":"thinking","thinking":"Weigh."}]}}]}),
                    ),
                ],
            );
            assert_eq!(
                contents(reduced.items()),
                [reasoning("Weigh."), text("answer")]
            );
        }
    }

    #[test]
    fn reasoning_details_accumulate_by_index_and_bind_as_one_sequence() {
        let frames = vec![
            delta(json!({"reasoning":"Weigh ","reasoning_details":[
                {"type":"reasoning.text","text":"Weigh ","format":"anthropic-claude-v1","index":0}]})),
            delta(json!({"reasoning":"both.","reasoning_details":[
                {"type":"reasoning.text","text":"both.","format":"anthropic-claude-v1","index":0}]})),
            delta(json!({"reasoning_details":[
                {"type":"reasoning.text","text":"","signature":"sig","index":0},
                {"type":"reasoning.encrypted","data":"opaque","id":"rs_1","index":1}]})),
            delta(json!({"content":"399."})),
            end("stop"),
        ];
        let reduced = decode_with(ReplayFormat::ChatReasoningDetail, frames.clone());
        assert_eq!(
            contents(reduced.items()),
            [reasoning("Weigh both."), reasoning(""), text("399.")]
        );
        let bound = Binding::Conversation;
        assert_eq!(
            native_replays(&reduced, ReplayFormat::ChatReasoningDetail),
            [
                (
                    json!({"type":"reasoning.text","text":"Weigh both.","format":"anthropic-claude-v1","signature":"sig","index":0}),
                    bound
                ),
                (
                    json!({"type":"reasoning.encrypted","data":"opaque","id":"rs_1","index":1}),
                    bound
                ),
            ]
        );
        // One signed detail binds the whole sequence, so unbinding drops it whole;
        // an unsigned sequence replays freely.
        let summary = json!({"type":"reasoning.summary","summary":"Sum","index":0});
        let encrypted = json!({"type":"reasoning.encrypted","data":"opaque","index":1});
        for (sequence, binding) in [
            (
                vec![summary.clone(), encrypted.clone()],
                Binding::Conversation,
            ),
            (vec![summary.clone()], Binding::Free),
        ] {
            let reduced = decode_with(
                ReplayFormat::ChatReasoningDetail,
                vec![
                    delta(json!({ "reasoning_details": sequence })),
                    delta(json!({"content":"399."})),
                    end("stop"),
                ],
            );
            let expected: Vec<_> = sequence
                .into_iter()
                .map(|detail| (detail, binding))
                .collect();
            assert_eq!(
                native_replays(&reduced, ReplayFormat::ChatReasoningDetail),
                expected
            );
            let message = crate::session::Message::Assistant(reduced.items().to_vec());
            let crate::session::Message::Assistant(unbound) = message.without_bound_reasoning()
            else {
                unreachable!()
            };
            let kept = unbound
                .iter()
                .filter(|item| item.replay().is_some())
                .count();
            assert_eq!(kept, if binding == Binding::Free { 1 } else { 0 });
        }
        // Without the convention, the `reasoning` alias is the text and details are ignored.
        let (items, _, _) = self::decode(frames);
        assert_eq!(contents(&items), [reasoning("Weigh both."), text("399.")]);
        assert_eq!(
            items[0].replay().unwrap().payload,
            json!({"text":"Weigh both."})
        );
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
    fn noop_packets_between_fragments_do_not_split_the_call() {
        let noops = || noop_packets().into_iter().map(event);
        let mut frames: Vec<_> = noops().collect();
        frames.push(tool_delta(call("call", "{\"path\":")));
        frames.extend(noops());
        frames.push(tool_delta(
            json!([{"index":0,"function":{"arguments":"\"file\"}"}}]),
        ));
        frames.push(end("tool_calls"));
        let (items, _, outcome) = decode(frames);
        assert_eq!(outcome, Outcome::ToolUse);
        assert_eq!(
            contents(&items),
            [tool("call", "inspect", json!({"path":"file"}))]
        );
    }

    #[test]
    fn unusable_tool_calls_are_rejected_at_the_finish() {
        for calls in [
            call("call", "{"),
            call("call", "[]"),
            call("call", "1"),
            json!([{"index":0,"function":{"arguments":"{}"}}]),
            // Non-string arguments are rejected, not emptied.
            json!([{"index":0,"id":"c","function":{"name":"rm","arguments":[1]}}]),
            json!([{"index":0,"id":"c","function":{"name":"rm","arguments":5}}]),
            json!([{"index":0,"id":"c","function":{"name":"rm","arguments":true}}]),
        ] {
            assert_fails_at(vec![tool_delta(calls), end("tool_calls")], 1);
        }
    }

    #[test]
    fn loose_tool_calls_are_repaired() {
        for (calls, expected) in [
            // Missing and duplicate IDs are synthesized so results still pair up.
            (
                json!([{"index":0,"id":"same","function":{"name":"one","arguments":"{}"}},
                       {"index":1,"id":"same","function":{"name":"two","arguments":"{}"}}]),
                vec![("same", "one", json!({})), (SYNTHETIC, "two", json!({}))],
            ),
            (
                json!([{"index":0,"function":{"name":"one","arguments":""}}]),
                vec![(SYNTHETIC, "one", json!({}))],
            ),
            // Null, empty, object, and double-encoded arguments; loose index/type.
            (
                json!([{"index":"0","type":"custom","id":"c","function":{"name":"one","arguments":"null"}}]),
                vec![("c", "one", json!({}))],
            ),
            (
                json!([{"index":null,"id":"c","function":{"name":"one","arguments":{"n":1}},"unknown":1}]),
                vec![("c", "one", json!({"n":1}))],
            ),
            (
                json!([{"id":"c","function":{"name":"one","arguments":"\"{\\\"n\\\":1}\""}}]),
                vec![("c", "one", json!({"n":1}))],
            ),
            // Names outside the Chat charset reach the runtime, which reports them.
            (
                json!([{"index":0,"id":"c","function":{"name":"server.tool","arguments":"{}"}}]),
                vec![("c", "server.tool", json!({}))],
            ),
        ] {
            assert_calls(vec![tool_delta(calls)], expected);
        }
    }

    #[test]
    fn synthesized_ids_never_collide_with_supplied_ones() {
        let calls = json!([{"index":0,"id":"call_1","function":{"name":"one","arguments":"{}"}},
                           {"index":1,"function":{"name":"two","arguments":"{}"}},
                           {"index":2,"id":"call_1","function":{"name":"three","arguments":"{}"}}]);
        let (items, _, _) = decode(vec![tool_delta(calls), end("tool_calls")]);
        let calls = calls_of(&items);
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].0, "call_1");
        let derived_for = |request: Value, response: Option<&str>, arguments: &str| {
            let mut chunk = json!({"choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"function":{"name":"x","arguments":arguments}}]}}]});
            if let Some(response) = response {
                chunk["id"] = json!(response);
            }
            let mut decoder = decoder().for_request(&request);
            let mut events = Vec::new();
            for frame in [event(chunk), end("tool_calls"), done()] {
                events.extend(decoder.decode(&frame).unwrap());
            }
            let reduced = crate::provider::codec::common::tests::reduce(events);
            reduced.items()[0].call().unwrap().id().to_owned()
        };
        let derived = |response, arguments| derived_for(json!({"turn":1}), response, arguments);
        let id = derived(Some("chatcmpl-x.y:z"), "{}");
        assert!(id.starts_with("call_") && id.ends_with("_0"), "{id}");
        assert!(
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')),
            "{id}"
        );
        assert_eq!(id, derived(Some("chatcmpl-x.y:z"), "{}"));
        assert_ne!(
            derived(Some("chatcmpl-9"), "{}"),
            derived(Some("chatcmpl-10"), "{}")
        );
        assert_eq!(derived(None, "{}"), derived(None, "{}"));
        assert_ne!(derived(None, "{}"), derived(None, "{\"a\":1}"));
        for response in [None, Some("constant")] {
            assert_ne!(
                derived_for(json!({"turn":1}), response, "{}"),
                derived_for(json!({"turn":2}), response, "{}")
            );
        }
    }

    #[test]
    fn unindexed_fragments_split_into_distinct_calls() {
        for (frames, expected) in [
            // A new name after a named call opens another call.
            (
                vec![
                    tool_delta(json!([{"id":"a","function":{"name":"one","arguments":""}}])),
                    tool_delta(json!([{"function":{"name":"two","arguments":"{}"}}])),
                ],
                vec![("a", "one", json!({})), (SYNTHETIC, "two", json!({}))],
            ),
            // A call without an ID followed by a named call with one.
            (
                vec![
                    tool_delta(json!([{"function":{"name":"one","arguments":"{}"}}])),
                    tool_delta(json!([{"id":"b","function":{"name":"two","arguments":"{}"}}])),
                ],
                vec![(SYNTHETIC, "one", json!({})), ("b", "two", json!({}))],
            ),
            // Two complete unindexed calls to the same tool within one delta.
            (
                vec![tool_delta(json!([
                    {"function":{"name":"same","arguments":"{\"n\":1}"}},
                    {"function":{"name":"same","arguments":"{\"n\":2}"}}
                ]))],
                vec![
                    (SYNTHETIC, "same", json!({"n":1})),
                    (SYNTHETIC, "same", json!({"n":2})),
                ],
            ),
            // Without indices, the full ID and name repeated around split
            // arguments continue the call; a new ID opens the next one.
            (
                vec![
                    tool_delta(json!([{"id":"a","function":{"name":"one","arguments":"{\"x\""}}])),
                    tool_delta(json!([{"id":"a","function":{"name":"one","arguments":":1}"}}])),
                    tool_delta(json!([{"id":"b","function":{"name":"two","arguments":"{}"}}])),
                ],
                vec![("a", "one", json!({"x":1})), ("b", "two", json!({}))],
            ),
            // A genuinely repeated name fragment is kept...
            (
                vec![
                    tool_delta(json!([{"index":0,"id":"g","function":{"name":"go"}}])),
                    tool_delta(json!([{"index":0,"function":{"name":"go"}}])),
                    tool_delta(json!([{"index":0,"function":{"arguments":"{}"}}])),
                ],
                vec![("g", "gogo", json!({}))],
            ),
            // ...but a name re-sent with every arguments chunk is a repeat.
            (
                vec![
                    tool_delta(
                        json!([{"index":0,"id":"c","function":{"name":"run","arguments":""}}]),
                    ),
                    tool_delta(json!([{"index":0,"function":{"name":"run","arguments":""}}])),
                    tool_delta(json!([{"index":0,"function":{"name":"run","arguments":"{}"}}])),
                ],
                vec![("c", "run", json!({}))],
            ),
            // Same-name unindexed calls in separate deltas: a new object after
            // a complete one opens a second call.
            (
                vec![
                    tool_delta(json!([{"function":{"name":"same","arguments":"{\"n\":1}"}}])),
                    tool_delta(json!([{"function":{"name":"same","arguments":"{\"n\":2}"}}])),
                ],
                vec![
                    (SYNTHETIC, "same", json!({"n":1})),
                    (SYNTHETIC, "same", json!({"n":2})),
                ],
            ),
            // Arbitrary wire indices never overflow.
            (
                vec![
                    tool_delta(
                        json!([{"index":u64::MAX,"id":"a","function":{"name":"one","arguments":"{}"}}]),
                    ),
                    tool_delta(json!([{"id":"b","function":{"name":"two","arguments":"{}"}}])),
                ],
                vec![("a", "one", json!({})), ("b", "two", json!({}))],
            ),
        ] {
            assert_calls(frames, expected);
        }
    }

    #[test]
    fn legacy_function_call_is_a_tool_call() {
        let (items, _, _) = decode(vec![
            delta(json!({"function_call":{"name":"legacy","arguments":"{\"y\":2}"}})),
            end("function_call"),
        ]);
        assert_eq!(
            calls_of(&items),
            [(SYNTHETIC.to_owned(), "legacy".to_owned(), json!({"y":2}))]
        );
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

    #[test]
    fn interleaved_tools_get_first_seen_ids_and_authoritative_arguments() {
        let mut decoder = decoder();
        let frames = [
            tool_delta(
                json!([{"index":7,"id":"call-","function":{"name":"fir","arguments":"{\"a\":"}},{"index":2,"id":"call-b","function":{"name":"second","arguments":"{"}}]),
            ),
            delta(json!({"content":"working"})),
            tool_delta(
                json!([{"index":2,"function":{"arguments":"}"}},{"index":7,"id":"a","function":{"name":"st","arguments":"1}"}}]),
            ),
            end("tool_calls"),
            done(),
        ];
        let mut events = Vec::new();
        for frame in frames {
            events.extend(decoder.decode(&frame).unwrap());
        }
        let fragments = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    ResponseEvent::Delta {
                        kind: ItemKind::ToolCall,
                        ..
                    }
                )
            })
            .count();
        let reduced = crate::provider::codec::common::tests::reduce(events);
        assert_eq!(
            (fragments, reduced.completion.outcome()),
            (4, Outcome::ToolUse)
        );
        let expected = [
            tool("call-a", "first", json!({"a":1})),
            tool("call-b", "second", json!({})),
            text("working"),
        ];
        assert_eq!(contents(reduced.items()), expected);
    }
}

//! Assemble visible text, reasoning envelopes, and sequential tool fragments.
use super::super::{valid_name, wire};
use super::{Block, Decoder};
use crate::provider::{
    ProviderError,
    backends::common::reasoning_envelope,
    protocol::{BlockContent, BlockKind, ContentDelta, ItemKind, ResponseChunk, ToolCall},
};
use serde_json::{Value, json};
use std::collections::BTreeSet;

impl Decoder {
    pub(super) fn delta(
        &mut self,
        delta: &wire::Delta,
        chunks: &mut Vec<ResponseChunk>,
    ) -> Result<(), ProviderError> {
        if delta.is_noop() {
            return Ok(());
        }
        // Null placeholders carry no semantics; reject non-null unknown fields
        // rather than silently losing audio, legacy function_call, etc.
        if delta.extra.values().any(|value| !value.is_null()) {
            return Err(ProviderError::protocol("Unsupported Chat delta field"));
        }
        if delta
            .role
            .as_deref()
            .is_some_and(|role| role != "assistant")
        {
            return Err(ProviderError::protocol("Chat delta role must be assistant"));
        }
        let primary = delta
            .reasoning_content
            .as_deref()
            .filter(|text| !text.is_empty());
        let alias = delta.reasoning.as_deref().filter(|text| !text.is_empty());
        if let (Some(primary), Some(alias)) = (primary, alias)
            && primary != alias
        {
            return Err(ProviderError::protocol(
                "Conflicting Chat reasoning delta aliases",
            ));
        }
        if let Some(text) = primary.or(alias) {
            self.visible_delta(text, true, chunks)?;
        }
        for text in [delta.content.as_deref(), delta.refusal.as_deref()]
            .into_iter()
            .flatten()
        {
            if !text.is_empty() {
                self.visible_delta(text, false, chunks)?;
            }
        }
        for call in delta.tool_calls.iter().flatten() {
            if call.kind.as_deref().is_some_and(|kind| kind != "function") {
                return Err(ProviderError::protocol(
                    "Only Chat function tool calls are supported",
                ));
            }
            // A Hermes delta may contain a header and argument fragments for
            // the same index. Apply each entry in wire order, not as a map.
            let id = if let Some(id) = self.tool_ids.get(&call.index) {
                *id
            } else {
                let id = self.blocks.len();
                self.blocks.push(Block::Tool {
                    call_id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                });
                self.tool_ids.insert(call.index, id);
                start_item(chunks, id, ItemKind::ToolCall, BlockKind::ToolCallArguments);
                id
            };
            let Block::Tool {
                call_id,
                name,
                arguments,
            } = &mut self.blocks[id]
            else {
                unreachable!()
            };
            if let Some(fragment) = &call.id {
                call_id.push_str(fragment);
            }
            if let Some(function) = &call.function {
                if let Some(fragment) = &function.name {
                    name.push_str(fragment);
                }
                if let Some(fragment) = &function.arguments {
                    arguments.push_str(fragment);
                    if !fragment.is_empty() {
                        chunks.push(ResponseChunk::BlockDelta {
                            item: id.to_string(),
                            block: "0".into(),
                            delta: ContentDelta::JsonFragment(fragment.clone()),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    fn visible_delta(
        &mut self,
        text: &str,
        reasoning: bool,
        chunks: &mut Vec<ResponseChunk>,
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
            if let Some(id) = self.visible_id.take() {
                let content = match &self.blocks[id] {
                    Block::Text(text) => BlockContent::Text { text: text.clone() },
                    Block::Reasoning(text) => BlockContent::Reasoning { text: text.clone() },
                    _ => unreachable!(),
                };
                self.end_item(chunks, id, content);
                self.ended.insert(id);
            }
            let id = self.blocks.len();
            self.blocks.push(if reasoning {
                Block::Reasoning(String::new())
            } else {
                Block::Text(String::new())
            });
            self.visible_id = Some(id);
            start_item(
                chunks,
                id,
                if reasoning {
                    ItemKind::Reasoning
                } else {
                    ItemKind::Text
                },
                if reasoning {
                    BlockKind::Reasoning
                } else {
                    BlockKind::Text
                },
            );
            id
        };
        match &mut self.blocks[id] {
            Block::Text(full) | Block::Reasoning(full) => full.push_str(text),
            _ => unreachable!(),
        }
        chunks.push(ResponseChunk::BlockDelta {
            item: id.to_string(),
            block: "0".into(),
            delta: ContentDelta::Text(text.into()),
        });
        Ok(())
    }

    pub(super) fn end_blocks(
        &self,
        chunks: &mut Vec<ResponseChunk>,
        discard_tools: bool,
    ) -> Result<(), ProviderError> {
        let mut call_ids = BTreeSet::new();
        for (id, block) in self.blocks.iter().enumerate() {
            if self.ended.contains(&id) {
                continue;
            }
            let block = match block {
                Block::Text(text) => BlockContent::Text { text: text.clone() },
                Block::Reasoning(text) => BlockContent::Reasoning { text: text.clone() },
                Block::Tool {
                    call_id,
                    name,
                    arguments,
                } => {
                    if discard_tools {
                        chunks.push(ResponseChunk::ItemDiscarded { id: id.to_string() });
                        continue;
                    }
                    if call_id.is_empty() || !call_ids.insert(call_id) || !valid_name(name) {
                        return Err(ProviderError::protocol(
                            "Chat tool calls require distinct nonempty IDs and valid function names",
                        ));
                    }
                    let arguments: Value = serde_json::from_str(arguments)
                        .map_err(|_| ProviderError::protocol("Invalid Chat tool arguments JSON"))?;
                    BlockContent::ToolCall(
                        ToolCall::new(call_id.clone(), name.clone(), arguments)
                            .map_err(|error| ProviderError::protocol(format!("Chat: {error}")))?,
                    )
                }
            };
            self.end_item(chunks, id, block);
        }
        Ok(())
    }
}

fn start_item(chunks: &mut Vec<ResponseChunk>, id: usize, kind: ItemKind, block_kind: BlockKind) {
    chunks.push(ResponseChunk::ItemStarted {
        id: id.to_string(),
        position: id,
        kind,
    });
    chunks.push(ResponseChunk::BlockStarted {
        item: id.to_string(),
        id: "0".into(),
        position: 0,
        kind: block_kind,
    });
}

impl Decoder {
    fn end_item(&self, chunks: &mut Vec<ResponseChunk>, id: usize, content: BlockContent) {
        // The transport wrapper binds the provider+endpoint scope. llama-swap
        // injected reasoning is indistinguishable on this wire and receives
        // the same origin scope, never a fabricated separate provenance.
        let replay = match &content {
            BlockContent::Reasoning { text } => Some(reasoning_envelope(
                "chat_completions",
                &self.model,
                json!({"text": text}),
            )),
            _ => None,
        };
        chunks.push(ResponseChunk::BlockEnded {
            item: id.to_string(),
            block: "0".into(),
            content,
        });
        chunks.push(ResponseChunk::ItemEnded {
            id: id.to_string(),
            replay,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Decoder, tests::*};
    use crate::provider::backends::transport::SseEvent;
    use crate::provider::protocol::{
        BlockContent, ContentDelta, ItemKind, ResponseAssembler, ResponseChunk, StopReason,
        ToolCall, Usage,
    };
    use serde_json::{Value, json};

    fn reasoning(text: &str) -> BlockContent {
        BlockContent::Reasoning { text: text.into() }
    }

    fn tool(id: &str, name: &str, arguments: Value) -> BlockContent {
        BlockContent::ToolCall(ToolCall::new(id, name, arguments).unwrap())
    }

    fn tool_delta(calls: Value) -> SseEvent {
        delta(json!({ "tool_calls": calls }))
    }

    fn call(id: &str, arguments: &str) -> Value {
        json!([{"index":0,"id":id,"function":{"name":"inspect","arguments":arguments}}])
    }

    /// Asserts frames before `failing` decode, and that frame and finish are rejected.
    fn assert_fails_at(frames: Vec<SseEvent>, failing: usize) {
        let mut decoder = Decoder::new("test-model".into());
        for frame in &frames[..failing] {
            decoder.decode(frame).unwrap();
        }
        let frame = &frames[failing];
        assert!(decoder.decode(frame).is_err(), "{}", frame.data);
        assert!(decoder.finish().is_err(), "{}", frame.data);
    }

    #[test]
    fn content_refusal_reasoning_and_tools_decode_as_ordered_items() {
        for (frames, expected_stop, expected) in [
            // A singleton choice without an index (llama loading) is reasoning.
            (
                vec![
                    event(json!({"choices":[{"delta":{"reasoning_content":"loading"}}]})),
                    end("stop"),
                ],
                StopReason::EndTurn,
                vec![reasoning("loading")],
            ),
            // Refusal is visible completed output.
            (
                vec![
                    delta(json!({"refusal":"I cannot "})),
                    delta(json!({"refusal":"help with that."})),
                    end("stop"),
                ],
                StopReason::EndTurn,
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
                StopReason::EndTurn,
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
                StopReason::ToolUse,
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
                StopReason::EndTurn,
                vec![
                    reasoning("first thought"),
                    text("hello "),
                    reasoning("second thought"),
                    text("世界"),
                ],
            ),
        ] {
            let (items, _, stop) = decode(frames);
            assert_eq!(stop, expected_stop);
            assert_eq!(contents(&items), expected.iter().collect::<Vec<_>>());
            for (position, item) in items.iter().enumerate() {
                assert_eq!(item.position, position);
                assert_eq!(item.replay.is_some(), item.kind == ItemKind::Reasoning);
            }
        }
    }

    #[test]
    fn unknown_non_null_delta_fields_are_errors_at_every_stage() {
        for field in ["unknown", "function_call", "audio"] {
            for value in [
                json!(""),
                json!(false),
                json!(0),
                json!([]),
                json!({}),
                json!({"data":"x"}),
            ] {
                let mut payload = json!({});
                payload[field] = value;
                for stage in 0..3 {
                    let prefix = [delta(json!({"content":"answer"})), end("stop")];
                    let mut frames: Vec<_> = prefix.into_iter().take(stage).collect();
                    frames.push(delta(payload.clone()));
                    assert_fails_at(frames, stage);
                }
            }
        }
    }

    #[test]
    fn noop_metadata_and_repeated_tool_finish_preserve_one_complete_tool() {
        let noops = || noop_packets().into_iter().map(event);
        let mut frames: Vec<_> = noops().collect();
        frames.push(tool_delta(call("call", "{\"path\":")));
        frames.extend(noops());
        frames.push(tool_delta(
            json!([{"index":0,"function":{"arguments":"\"file\"}"}}]),
        ));
        frames.push(end("tool_calls"));
        frames.extend(noops());
        frames.extend([
            event(json!({"choices":[{"finish_reason":"tool_calls","delta":{"role":"assistant","tool_calls":[]}}]})),
            event(phantom_usage_chunk()),
            event(json!({"choices":[{"finish_reason":"tool_calls","delta":null}]})),
            done(),
        ]);
        let (items, usage, stop) = decode(frames);
        assert_eq!((stop, usage), (StopReason::ToolUse, USAGE));
        assert_eq!(
            contents(&items),
            [&tool("call", "inspect", json!({"path":"file"}))]
        );
    }

    #[test]
    fn abnormal_finishes_discard_tools_once_regardless_of_argument_completeness() {
        for (finish, expected_stop) in [
            ("length", StopReason::MaxTokens),
            ("abort", StopReason::Aborted),
            ("content_filter", StopReason::ContentFilter),
        ] {
            for arguments in ["{", "{}", "not json", "[]"] {
                for repeated in [false, true] {
                    let mut frames = vec![
                        delta(json!({"content":"partial","tool_calls":call("call", arguments)})),
                        end(finish),
                    ];
                    if repeated {
                        frames.extend([
                            event(json!({"choices":[{"finish_reason":finish,"delta":{"role":"assistant","tool_calls":[]}}]})),
                            event(phantom_usage_chunk()),
                            event(json!({"choices":[{"finish_reason":finish}]})),
                            done(),
                        ]);
                    }
                    let (items, usage, stop) = decode(frames);
                    let expected_usage = if repeated { USAGE } else { Usage::default() };
                    assert_eq!((stop, usage), (expected_stop.clone(), expected_usage));
                    assert_eq!(contents(&items), [&text("partial")]);
                }
            }
        }
    }

    #[test]
    fn invalid_tools_and_reasoning_aliases_are_rejected_at_the_offending_frame() {
        let mut cases = Vec::new();
        // Normal finish requires complete valid tools, headers and distinct call IDs.
        for calls in [
            call("call", "{"),
            call("call", "[]"),
            call("call", "null"),
            json!([{"index":0,"id":"same","function":{"name":"one","arguments":"{}"}},
                   {"index":1,"id":"same","function":{"name":"two","arguments":"{}"}}]),
            json!([{"index":0,"function":{"arguments":"{}"}}]),
            json!([{"index":0,"id":"call","function":{"name":"invalid name","arguments":"{}"}}]),
        ] {
            cases.push((vec![tool_delta(calls), end("tool_calls")], 1));
        }
        for call in [
            json!({"index":null}),
            json!({"index":"0"}),
            json!({"index":0,"type":"custom"}),
            json!({"index":0,"function":{"arguments":{}}}),
            json!({"index":0,"unknown":1}),
        ] {
            cases.push((vec![tool_delta(json!([call]))], 0));
        }
        for value in [
            json!({"reasoning":"a","reasoning_content":"b"}),
            json!({"reasoning":{}}),
            json!({"content":[]}),
        ] {
            cases.push((vec![delta(value)], 0));
        }
        for (frames, failing) in cases {
            assert_fails_at(frames, failing);
        }
    }

    #[test]
    fn interleaved_tools_get_first_seen_ids_and_authoritative_arguments() {
        let mut decoder = Decoder::new("gpt-5".into());
        let mut assembler = ResponseAssembler::default();
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
        let mut fragments = 0;
        for frame in frames {
            for chunk in decoder.decode(&frame).unwrap() {
                if matches!(
                    chunk,
                    ResponseChunk::BlockDelta {
                        delta: ContentDelta::JsonFragment(_),
                        ..
                    }
                ) {
                    fragments += 1;
                }
                assembler.push(&chunk).unwrap();
            }
        }
        let (items, _, reason) = assembler.finish().unwrap();
        assert_eq!((fragments, reason), (4, StopReason::ToolUse));
        let expected = [
            tool("call-a", "first", json!({"a":1})),
            tool("call-b", "second", json!({})),
            text("working"),
        ];
        assert_eq!(contents(&items), expected.iter().collect::<Vec<_>>());
    }
}

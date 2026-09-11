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
                    if !arguments.is_object() {
                        return Err(ProviderError::protocol(
                            "Chat tool arguments must be a JSON object",
                        ));
                    }
                    BlockContent::ToolCall(ToolCall {
                        id: call_id.clone(),
                        name: name.clone(),
                        arguments,
                    })
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
    use crate::provider::protocol::{BlockContent, ItemKind, StopReason, Usage};
    use crate::provider::protocol::{ContentDelta, ResponseChunk, ToolCall};
    use serde_json::json;

    #[test]
    fn singleton_absent_index_llama_loading_is_reasoning() {
        let (items, _, _) = decode(vec![
            event(json!({"choices":[{"delta":{"reasoning_content":"loading"}}]})),
            end("stop"),
        ]);
        assert_eq!(
            items[0].blocks[0].content,
            BlockContent::Reasoning {
                text: "loading".into()
            }
        );
    }

    #[test]
    fn hermes_repeated_index_header_and_arguments_are_sequential() {
        let (items, _, reason) = decode(vec![
            delta(json!({"tool_calls":[
                {"index":0,"id":"call-a","type":"function","function":{"name":"inspect","arguments":""}},
                {"index":0,"function":{"arguments":"{\"path\":"}},
                {"index":1,"id":"call-b","function":{"name":"other","arguments":"{}"}},
                {"index":0,"function":{"arguments":"\"file\"}"}}
            ]})),
            end("tool_calls"),
        ]);
        assert_eq!(reason, StopReason::ToolUse);
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].blocks[0].content,
            BlockContent::ToolCall(ToolCall {
                id: "call-a".into(),
                name: "inspect".into(),
                arguments: json!({"path":"file"}),
            })
        );
    }

    #[test]
    fn refusal_is_visible_completed_output() {
        let (items, _, reason) = decode(vec![
            delta(json!({"refusal":"I cannot "})),
            delta(json!({"refusal":"help with that."})),
            end("stop"),
        ]);
        assert_eq!(reason, StopReason::EndTurn);
        assert_eq!(
            items[0].blocks[0].content,
            BlockContent::Text {
                text: "I cannot help with that.".into()
            }
        );
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
                for stage in 0..3 {
                    let mut decoder = Decoder::new("test-model".into());
                    if stage > 0 {
                        decoder.decode(&delta(json!({"content":"answer"}))).unwrap();
                    }
                    if stage > 1 {
                        decoder.decode(&end("stop")).unwrap();
                    }
                    let mut payload = json!({});
                    payload[field] = value.clone();
                    assert!(
                        decoder.decode(&delta(payload)).is_err(),
                        "{stage}: {field}={value}"
                    );
                    assert!(decoder.finish().is_err());
                }
            }
        }
    }

    #[test]
    fn noop_metadata_and_repeated_tool_finish_preserve_one_complete_tool() {
        let mut frames = vec![];
        frames.extend(noop_packets().into_iter().map(event));
        frames.push(delta(json!({"tool_calls":[{"index":0,"id":"call","function":{"name":"inspect","arguments":"{\"path\":"}}]})));
        frames.extend(noop_packets().into_iter().map(event));
        frames.push(delta(
            json!({"tool_calls":[{"index":0,"function":{"arguments":"\"file\"}"}}]}),
        ));
        frames.push(end("tool_calls"));
        frames.extend(noop_packets().into_iter().map(event));
        frames.push(event(json!({"choices":[{"finish_reason":"tool_calls","delta":{"role":"assistant","tool_calls":[]}}]})));
        frames.push(event(phantom_usage_chunk()));
        frames.push(event(
            json!({"choices":[{"finish_reason":"tool_calls","delta":null}]}),
        ));
        frames.push(done());
        let (items, usage, stop) = decode(frames);
        assert_eq!(stop, StopReason::ToolUse);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].blocks[0].content,
            BlockContent::ToolCall(ToolCall {
                id: "call".into(),
                name: "inspect".into(),
                arguments: json!({"path":"file"}),
            })
        );
        assert_eq!(
            usage,
            Usage {
                input_tokens: 20,
                cached_input_tokens: 80,
                output_tokens: 10
            }
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
                        delta(
                            json!({"content":"partial","tool_calls":[{"index":0,"id":"call","function":{"name":"inspect","arguments":arguments}}]}),
                        ),
                        end(finish),
                    ];
                    if repeated {
                        frames.extend([
                            event(json!({"choices":[{"finish_reason":finish,"delta":{"role":"assistant","tool_calls":[]}}]})),
                            event(phantom_usage_chunk()),
                            event(json!({"choices":[{"finish_reason":finish}]})), done(),
                        ]);
                    }
                    let (items, usage, stop) = decode(frames);
                    assert_eq!(stop, expected_stop);
                    assert_eq!(items.len(), 1);
                    assert_eq!(
                        items[0].blocks[0].content,
                        BlockContent::Text {
                            text: "partial".into()
                        }
                    );
                    assert_eq!(
                        usage,
                        if repeated {
                            Usage {
                                input_tokens: 20,
                                cached_input_tokens: 80,
                                output_tokens: 10,
                            }
                        } else {
                            Usage::default()
                        }
                    );
                }
            }
        }
    }

    #[test]
    fn normal_finish_requires_complete_valid_tools() {
        for arguments in ["{", "[]", "null"] {
            let mut decoder = Decoder::new("test-model".into());
            decoder.decode(&delta(json!({"tool_calls":[{"index":0,"id":"call","function":{"name":"inspect","arguments":arguments}}]}))).unwrap();
            assert!(decoder.decode(&end("tool_calls")).is_err());
            assert!(decoder.finish().is_err());
        }
    }

    #[test]
    fn reasoning_aliases_are_validated_and_not_duplicated() {
        let (items, _, _) = decode(vec![
            delta(json!({"reasoning":"same","reasoning_content":"same"})),
            end("stop"),
        ]);
        assert_eq!(
            items[0].blocks[0].content,
            BlockContent::Reasoning {
                text: "same".into()
            }
        );
        for value in [
            json!({"reasoning":"a","reasoning_content":"b"}),
            json!({"reasoning":{}}),
            json!({"content":[]}),
            json!({"audio":{"data":"x"}}),
        ] {
            let mut decoder = Decoder::new("test-model".into());
            assert!(decoder.decode(&delta(value)).is_err());
        }
        decode(vec![
            delta(json!({"role":null,"content":null,"reasoning":null,"function_call":null})),
            end("stop"),
        ]);
    }

    #[test]
    fn sequential_tool_updates_still_validate_headers_and_distinct_call_ids() {
        for calls in [
            json!([{"index":0,"id":"same","function":{"name":"one","arguments":"{}"}},
                   {"index":1,"id":"same","function":{"name":"two","arguments":"{}"}}]),
            json!([{"index":0,"function":{"arguments":"{}"}}]),
            json!([{"index":0,"id":"call","function":{"name":"invalid name","arguments":"{}"}}]),
        ] {
            let mut decoder = Decoder::new("test-model".into());
            decoder.decode(&delta(json!({"tool_calls":calls}))).unwrap();
            assert!(decoder.decode(&end("tool_calls")).is_err());
        }
        for call in [
            json!({"index":null}),
            json!({"index":"0"}),
            json!({"index":0,"type":"custom"}),
            json!({"index":0,"function":{"arguments":{}}}),
            json!({"index":0,"unknown":1}),
        ] {
            let mut decoder = Decoder::new("test-model".into());
            assert!(
                decoder
                    .decode(&delta(json!({"tool_calls":[call]})))
                    .is_err()
            );
        }
    }

    #[test]
    fn interleaved_tools_get_first_seen_ids_and_authoritative_arguments() {
        use crate::provider::protocol::ResponseAssembler;
        let mut decoder = Decoder::new("gpt-5".into());
        let mut assembler = ResponseAssembler::default();
        let frames = [
            delta(
                json!({"tool_calls":[{"index":7,"id":"call-","function":{"name":"fir","arguments":"{\"a\":"}},{"index":2,"id":"call-b","function":{"name":"second","arguments":"{"}}]}),
            ),
            delta(json!({"content":"working"})),
            delta(
                json!({"tool_calls":[{"index":2,"function":{"arguments":"}"}},{"index":7,"id":"a","function":{"name":"st","arguments":"1}"}}]}),
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
        assert_eq!(fragments, 4);
        assert_eq!(reason, StopReason::ToolUse);
        assert_eq!(
            items[0].blocks[0].content,
            BlockContent::ToolCall(ToolCall {
                id: "call-a".into(),
                name: "first".into(),
                arguments: json!({"a":1})
            })
        );
        assert_eq!(
            items[1].blocks[0].content,
            BlockContent::ToolCall(ToolCall {
                id: "call-b".into(),
                name: "second".into(),
                arguments: json!({})
            })
        );
    }
    #[test]
    fn alternating_reasoning_and_text_remain_separate_and_ordered() {
        let (items, _, _) = decode(vec![
            delta(json!({"role":"assistant", "content":null, "reasoning_content":"first thought"})),
            delta(json!({"content":"hello "})),
            delta(json!({"reasoning":"second thought"})),
            delta(json!({"content":"世界"})),
            end("stop"),
        ]);
        let contents: Vec<_> = items.iter().map(|item| &item.blocks[0].content).collect();
        assert_eq!(
            contents,
            vec![
                &BlockContent::Reasoning {
                    text: "first thought".into()
                },
                &BlockContent::Text {
                    text: "hello ".into()
                },
                &BlockContent::Reasoning {
                    text: "second thought".into()
                },
                &BlockContent::Text {
                    text: "世界".into()
                },
            ]
        );
        for (position, item) in items.iter().enumerate() {
            assert_eq!(item.position, position);
            assert_eq!(item.replay.is_some(), item.kind == ItemKind::Reasoning);
        }
    }
}
